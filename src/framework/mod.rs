//! The central Framework struct that ties everything together.

// Prefix and slash specific implementation details
mod prefix;
mod slash;

use crate::serenity_prelude as serenity;
use crate::*;

async fn check_permissions<U, E>(
    ctx: crate::Context<'_, U, E>,
    required_permissions: serenity::Permissions,
) -> bool {
    if required_permissions.is_empty() {
        return true;
    }

    let guild_id = match ctx.guild_id() {
        Some(x) => x,
        None => return true, // no permission checks in DMs
    };

    let guild = match ctx.discord().cache.guild(guild_id) {
        Some(x) => x,
        None => return false, // Guild not in cache
    };

    let channel = match guild.channels.get(&ctx.channel_id()) {
        Some(serenity::Channel::Guild(channel)) => channel,
        Some(_other_channel) => {
            println!(
                "Warning: guild message was supposedly sent in a non-guild channel. Denying invocation"
            );
            return false;
        }
        None => return false,
    };

    // If member not in cache (probably because presences intent is not enabled), retrieve via HTTP
    let member = match guild.members.get(&ctx.author().id) {
        Some(x) => x.clone(),
        None => match ctx
            .discord()
            .http
            .get_member(guild_id.0, ctx.author().id.0)
            .await
        {
            Ok(member) => member,
            Err(_) => return false,
        },
    };

    match guild.user_permissions_in(channel, &member) {
        Ok(perms) => perms.contains(required_permissions),
        Err(_) => false,
    }
}

async fn check_required_permissions_and_owners_only<U, E>(
    ctx: crate::Context<'_, U, E>,
    required_permissions: serenity::Permissions,
    owners_only: bool,
) -> bool {
    if owners_only && !ctx.framework().options().owners.contains(&ctx.author().id) {
        return false;
    }

    if !check_permissions(ctx, required_permissions).await {
        return false;
    }

    true
}

pub struct Framework<U, E> {
    prefix: String,
    user_data: once_cell::sync::OnceCell<U>,
    user_data_setup: std::sync::Mutex<
        Option<
            Box<
                dyn Send
                    + Sync
                    + for<'a> FnOnce(
                        &'a serenity::Context,
                        &'a serenity::Ready,
                        &'a Self,
                    ) -> BoxFuture<'a, Result<U, E>>,
            >,
        >,
    >,
    // The bot ID is embedded in the token so we shouldn't have to do all of this mutex mess
    // But it's kinda messy to get access to the token in the framework
    bot_id: std::sync::Mutex<Option<serenity::UserId>>,
    // TODO: wrap in RwLock to allow changing framework options while running? Could also replace
    // the edit tracking cache interior mutability
    options: FrameworkOptions<U, E>,
    application_id: serenity::ApplicationId,
}

impl<U, E> Framework<U, E> {
    /// Setup a new blank Framework with a prefix and a callback to provide user data.
    ///
    /// The user data callback is invoked as soon as the bot is logged. That way, bot data like user
    /// ID or connected guilds can be made available to the user data setup function. The user data
    /// setup is not allowed to return Result because there would be no reasonable
    /// course of action on error.
    pub fn new<F>(
        prefix: String,
        application_id: serenity::ApplicationId,
        user_data_setup: F,
        options: FrameworkOptions<U, E>,
    ) -> Self
    where
        F: Send
            + Sync
            + 'static
            + for<'a> FnOnce(
                &'a serenity::Context,
                &'a serenity::Ready,
                &'a Self,
            ) -> BoxFuture<'a, Result<U, E>>,
    {
        Self {
            prefix,
            user_data: once_cell::sync::OnceCell::new(),
            user_data_setup: std::sync::Mutex::new(Some(Box::new(user_data_setup))),
            bot_id: std::sync::Mutex::new(None),
            options,
            application_id,
        }
    }

    pub async fn start(self, builder: serenity::ClientBuilder<'_>) -> Result<(), serenity::Error>
    where
        U: Send + Sync + 'static,
        E: 'static + Send,
    {
        let application_id = self.application_id;

        let self_1 = std::sync::Arc::new(self);
        let self_2 = std::sync::Arc::clone(&self_1);

        let edit_track_cache_purge_task = tokio::spawn(async move {
            loop {
                if let Some(edit_tracker) = &self_1.options.prefix_options.edit_tracker {
                    edit_tracker.write().purge();
                }
                // not sure if the purging interval should be configurable
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });

        let event_handler = EventWrapper(move |ctx, event| {
            let self_2 = std::sync::Arc::clone(&self_2);
            Box::pin(async move {
                self_2.event(ctx, event).await;
            }) as _
        });
        builder
            .application_id(application_id.0)
            .event_handler(event_handler)
            .await?
            .start()
            .await?;

        edit_track_cache_purge_task.abort();

        Ok(())
    }

    pub fn options(&self) -> &FrameworkOptions<U, E> {
        &self.options
    }

    pub fn application_id(&self) -> serenity::ApplicationId {
        self.application_id
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    async fn get_user_data(&self) -> &U {
        // We shouldn't get a Message event before a Ready event. But if we do, wait until
        // the Ready event does come and the resulting data has arrived.
        loop {
            match self.user_data.get() {
                Some(x) => break x,
                None => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
    }

    async fn event(&self, ctx: serenity::Context, event: Event<'_>)
    where
        U: Send + Sync,
    {
        match &event {
            Event::Ready { data_about_bot } => {
                *self.bot_id.lock().unwrap() = Some(data_about_bot.user.id);

                let user_data_setup = Option::take(&mut *self.user_data_setup.lock().unwrap());
                if let Some(user_data_setup) = user_data_setup {
                    match user_data_setup(&ctx, data_about_bot, self).await {
                        Ok(user_data) => {
                            let _: Result<_, _> = self.user_data.set(user_data);
                        }
                        Err(e) => (self.options.on_error)(e, ErrorContext::Setup).await,
                    }
                } else {
                    // discarding duplicate Discord bot ready event
                    // (happens regularly when bot is online for long period of time)
                }
            }
            Event::Message { new_message } => {
                if let Err(Some((err, ctx))) =
                    prefix::dispatch_message(self, &ctx, new_message, false).await
                {
                    if let Some(on_error) = ctx.command.options.on_error {
                        (on_error)(err, ctx).await;
                    } else {
                        (self.options.on_error)(
                            err,
                            crate::ErrorContext::Command(crate::CommandErrorContext::Prefix(ctx)),
                        )
                        .await;
                    }
                }
            }
            Event::MessageUpdate { event, .. } => {
                if let Some(edit_tracker) = &self.options.prefix_options.edit_tracker {
                    let msg = edit_tracker.write().process_message_update(event);

                    if let Err(Some((err, ctx))) =
                        prefix::dispatch_message(self, &ctx, &msg, true).await
                    {
                        (self.options.on_error)(
                            err,
                            crate::ErrorContext::Command(crate::CommandErrorContext::Prefix(ctx)),
                        )
                        .await;
                    }
                }
            }
            Event::MessageDelete {
                deleted_message_id, ..
            } => {
                if let Some(edit_tracker) = &self.options.prefix_options.edit_tracker {
                    let bot_response = edit_tracker
                        .write()
                        .find_bot_response(*deleted_message_id)
                        .cloned();
                    if let Some(bot_response) = bot_response {
                        if let Err(e) = bot_response.delete(&ctx).await {
                            println!(
                                "Warning: couldn't delete bot response when user deleted message: {}",
                                e
                            );
                        }
                    }
                }
            }
            Event::InteractionCreate {
                interaction: serenity::Interaction::ApplicationCommand(interaction),
            } => {
                if let Err((e, error_ctx)) = slash::dispatch_interaction(
                    self,
                    &ctx,
                    interaction,
                    &interaction.data.name,
                    &interaction.data.options,
                    &std::sync::atomic::AtomicBool::new(false),
                )
                .await
                {
                    if let Some(on_error) = error_ctx.command.options.on_error {
                        on_error(e, error_ctx).await;
                    } else {
                        (self.options.on_error)(
                            e,
                            ErrorContext::Command(CommandErrorContext::Slash(error_ctx)),
                        )
                        .await;
                    }
                }
            }
            _ => {}
        }

        // Do this after the framework's Ready handling, so that self.get_user_data() doesnt
        // potentially block infinitely
        if let Err(e) =
            (self.options.listener)(&ctx, &event, self, self.get_user_data().await).await
        {
            (self.options.on_error)(e, ErrorContext::Listener(&event));
        }
    }
}
