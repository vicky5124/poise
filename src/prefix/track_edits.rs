//! Tools for implementing automatic edit tracking, i.e. the bot automatically updating its response
//! when the user edits their command invocation message.

use crate::serenity_prelude as serenity;

fn update_message(message: &mut serenity::Message, update: serenity::MessageUpdateEvent) {
    message.id = update.id;
    message.channel_id = update.channel_id;
    message.guild_id = update.guild_id;

    if let Some(kind) = update.kind {
        message.kind = kind;
    }
    if let Some(content) = update.content {
        message.content = content;
    }
    if let Some(tts) = update.tts {
        message.tts = tts;
    }
    if let Some(pinned) = update.pinned {
        message.pinned = pinned;
    }
    if let Some(timestamp) = update.timestamp {
        message.timestamp = timestamp;
    }
    if let Some(edited_timestamp) = update.edited_timestamp {
        message.edited_timestamp = Some(edited_timestamp);
    }
    if let Some(author) = update.author {
        message.author = author;
    }
    if let Some(mention_everyone) = update.mention_everyone {
        message.mention_everyone = mention_everyone;
    }
    if let Some(mentions) = update.mentions {
        message.mentions = mentions;
    }
    if let Some(mention_roles) = update.mention_roles {
        message.mention_roles = mention_roles;
    }
    if let Some(attachments) = update.attachments {
        message.attachments = attachments;
    }
    // if let Some(embeds) = update.embeds {
    //     message.embeds = embeds;
    // }
}

pub struct EditTracker {
    max_duration: std::time::Duration,
    cache: Vec<(serenity::Message, serenity::Message)>,
}

impl EditTracker {
    pub fn for_timespan(duration: std::time::Duration) -> parking_lot::RwLock<Self> {
        parking_lot::RwLock::new(Self {
            max_duration: duration,
            cache: Vec::new(),
        })
    }

    /// Returns a copy of a newly up-to-date cached message, or a brand new generated message when
    /// not in cache
    pub fn process_message_update(
        &mut self,
        user_msg_update: &serenity::MessageUpdateEvent,
    ) -> serenity::Message {
        match self
            .cache
            .iter_mut()
            .find(|(user_msg, _)| user_msg.id == user_msg_update.id)
        {
            Some((user_msg, _)) => {
                update_message(user_msg, user_msg_update.clone());
                user_msg.clone()
            }
            None => {
                let mut user_msg = serenity::CustomMessage::new().build();
                update_message(&mut user_msg, user_msg_update.clone());
                user_msg
            }
        }
    }

    pub fn purge(&mut self) {
        let max_duration = self.max_duration;
        self.cache.retain(|(user_msg, _)| {
            let last_update = user_msg.edited_timestamp.unwrap_or(user_msg.timestamp);
            if let Ok(age) = (chrono::Utc::now() - last_update).to_std() {
                age < max_duration
            } else {
                false
            }
        });
    }

    pub fn find_bot_response(
        &mut self,
        user_msg_id: serenity::MessageId,
    ) -> Option<&mut serenity::Message> {
        let (_, bot_response) = self
            .cache
            .iter_mut()
            .find(|(user_msg, _)| user_msg.id == user_msg_id)?;
        Some(bot_response)
    }

    /// Returns a reference to the bot response that was just registered
    fn register_response(&mut self, user_msg: serenity::Message, bot_response: serenity::Message) {
        self.cache.push((user_msg, bot_response));
    }
}

pub async fn send_prefix_reply<U, E>(
    ctx: crate::prefix::PrefixContext<'_, U, E>,
    builder: impl for<'a, 'b> FnOnce(&'a mut crate::CreateReply<'b>) -> &'a mut crate::CreateReply<'b>,
) -> Result<(), serenity::Error> {
    let mut reply = crate::CreateReply::default();
    builder(&mut reply);
    let crate::CreateReply {
        content,
        embed,
        attachments,
        ephemeral: _,
    } = reply;

    let lock_edit_tracker = || {
        if let Some(command) = ctx.command {
            if !command.options.track_edits {
                return None;
            }
        }

        ctx.framework
            .options()
            .prefix_options
            .edit_tracker
            .as_ref()
            .map(|t| t.write())
    };

    let existing_response = lock_edit_tracker()
        .as_mut()
        .and_then(|t| t.find_bot_response(ctx.msg.id))
        .cloned();

    if let Some(mut response) = existing_response {
        response
            .edit(ctx.discord, |f| {
                // Empty string resets content (happens when user replaces text with embed)
                f.content(content.as_deref().unwrap_or(""));

                match embed {
                    Some(embed) => f.set_embed(embed),
                    None => f.set_embeds(Vec::new()),
                };

                f.0.insert("attachments", serde_json::json! { [] }); // reset attachments
                for attachment in attachments {
                    f.attachment(attachment);
                }

                f
            })
            .await?;

        // If the entry still exists after the await, update it to the new contents
        if let Some(response_entry) = lock_edit_tracker()
            .as_mut()
            .and_then(|t| t.find_bot_response(ctx.msg.id))
        {
            *response_entry = response;
        }
    } else {
        let new_response = ctx
            .msg
            .channel_id
            .send_message(ctx.discord, |m| {
                if let Some(content) = content {
                    m.content(content);
                }
                if let Some(embed) = embed {
                    m.set_embed(embed);
                }
                if let Some(allowed_mentions) = &ctx.framework.options().allowed_mentions {
                    m.allowed_mentions(|m| {
                        *m = allowed_mentions.clone();
                        m
                    });
                }

                for attachment in attachments {
                    m.add_file(attachment);
                }
                m
            })
            .await?;
        if let Some(track_edits) = &mut lock_edit_tracker() {
            track_edits.register_response(ctx.msg.clone(), new_response);
        }
    }
    Ok(())
}

pub async fn say_prefix_reply<U, E>(
    ctx: crate::prefix::PrefixContext<'_, U, E>,
    text: String,
) -> Result<(), serenity::Error> {
    send_prefix_reply(ctx, |m| m.content(text)).await
}
