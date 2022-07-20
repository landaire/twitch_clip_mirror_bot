#![feature(let_chains)]

use config::{load_config, write_config, Config, ServerConfig};
use futures_util::StreamExt;
use hyper::client::{Client as HyperClient, HttpConnector};
use log::warn;
use twilight_model::guild::{PartialMember, Permissions, Role};

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::{
    collections::HashMap,
    env,
    future::Future,
    io::Write,
    process::Stdio,
    sync::{Arc, RwLock},
};
use tokio::io::AsyncWriteExt;
use tracing::info;
use twilight_gateway::{Event, Intents, Shard};
use twilight_http::api_error::{ApiError, GeneralApiError};
use twilight_http::error::ErrorType;
use twilight_http::Client as HttpClient;
use twilight_model::{
    channel::{Channel, Message},
    http::attachment::Attachment,
    id::{
        marker::{ChannelMarker, GuildMarker},
        Id,
    },
};
use twilight_standby::Standby;

mod config;
mod twitch;

type State = Arc<StateRef>;

#[derive(Debug)]
struct StateRef {
    http: HttpClient,
    hyper: HyperClient<HttpConnector>,
    shard: Shard,
    standby: Standby,
    server_config: RwLock<Config>,
    my_id: AtomicU64,
}

fn spawn(fut: impl Future<Output = anyhow::Result<()>> + Send + 'static) {
    tokio::spawn(async move {
        if let Err(why) = fut.await {
            tracing::info!("handler error: {why:?}");
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(feature = "console")]
    console_subscriber::init();

    // Initialize the tracing subscriber.
    #[cfg(not(feature = "console"))]
    tracing_subscriber::fmt::init();

    let config: Config = load_config();

    let (mut events, state) = {
        let token = env::var("DISCORD_TOKEN")?;
        let http = twilight_http::Client::builder()
            .token(token.clone())
            .timeout(Duration::from_secs(30))
            .build();

        let intents = Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
        let (shard, events) = Shard::new(token, intents).await?;

        shard.start().await?;

        (
            events,
            Arc::new(StateRef {
                http,
                hyper: HyperClient::new(),
                shard,
                standby: Standby::new(),
                server_config: RwLock::new(config),
                my_id: AtomicU64::new(0),
            }),
        )
    };

    while let Some(event) = events.next().await {
        state.standby.process(&event);

        match event {
            Event::Ready(ready) => {
                state.my_id.store(ready.user.id.into(), Ordering::Relaxed);
                continue;
            }
            Event::MessageCreate(msg) => {
                if is_command(&msg) {
                    spawn(handle_command(msg.0, Arc::clone(&state)));
                    continue;
                }

                if msg.guild_id.is_none()
                    || msg.author.bot
                    || msg.author.id == state.my_id.load(Ordering::Relaxed)
                {
                    continue;
                }

                let server_id = msg.guild_id.unwrap();

                let config = state.server_config.read().unwrap();
                let server_config = config.servers.iter().find(|server| server.id == server_id);
                if server_config.is_none() {
                    continue;
                }

                let server_config = server_config.unwrap();

                let should_post_in_channel = server_config
                    .monitored_channels
                    .iter()
                    .any(|channel| channel.id == msg.channel_id);

                if !should_post_in_channel {
                    continue;
                }

                // let cached_channels = channels.read().expect("lock poisoned");
                // if let Some(channel_name) = cached_channels
                //     .get(msg.guild_id.as_ref().unwrap())
                //     .map(|guild_channels| guild_channels.get(&msg.channel_id))
                //     .flatten()
                // {
                //     if channel_name != "new-twitch-clips-for-soundboard" {
                //         continue;
                //     }
                // } else {
                //     // we don't know about this channel?
                //     continue;
                // }

                let clip_regex =
                    regex::Regex::new(r#"https://(m\.)?clips\.twitch\.tv/[^\s"']+"#).unwrap();
                if clip_regex.is_match(&msg.content) {
                    spawn(mirror(msg.0, Arc::clone(&state)));
                    continue;
                }
            }
            Event::MemberUpdate(update) => {
                // spawn(update_channels(update.guild_id, Arc::clone(&state)));
                continue;
            }
            _ => {
                // do not care
            }
        }
    }

    Ok(())
}

async fn mirror(msg: Message, state: State) -> anyhow::Result<()> {
    let clip_regex = regex::Regex::new(r#"https://(m\.)?clips\.twitch\.tv/[^\s"]+"#).unwrap();
    let clip_title_sanitization = regex::Regex::new(r"[^a-zA-Z0-9_-]").unwrap();

    for cap in clip_regex.captures_iter(&msg.content) {
        let clip_url = &cap[0];
        info!("Downloading {}", clip_url);

        let mut clip = twitch::download_clip(clip_url).await;
        if clip.is_none() {
            // the clip was empty, we need to try getting its details from the message
            // embed content
            for embed in &msg.embeds {
                if let Some(provider) = &embed.provider &&
                    let Some("Twitch") = provider.name.as_ref().map(String::as_str) &&
                        let Some(title) = &embed.title &&
                        let Some(thumbnail) = &embed.thumbnail {
                            clip = twitch::download_clip_from_thumbnail_url(
                                thumbnail.url.as_str(),
                            )
                            .await
                            .map(|(contents, video_url)| (title.clone(), contents, video_url));
                            break;
                        }
            }
        }

        if clip.is_none() {
            // Try downloading based off of the discord rich preview metadata
            state
                .http
                .create_message(msg.channel_id)
                .reply(msg.id)
                .content(&format!("Could not download {:?}", clip_url))?
                .exec()
                .await?;
            continue;
        }

        info!("Downloaded!");

        let clip = clip.unwrap();
        info!("{} title: {}", clip_url, &clip.0);
        let title = clip_title_sanitization.replace_all(clip.0.as_str(), "_");
        info!("sanitized: {}", title);
        let audio_attachment = Attachment::from_bytes(
            format!("{}.mp3", title),
            extract_audio(clip.1.as_slice()).await,
            2,
        );
        let video_too_large = clip.1.len() > 8 * 1000 * 1000;
        let video_attachment = Attachment::from_bytes(format!("{}.mp4", title), clip.1, 1);

        info!("Sending message...");

        let mirrored_channel = {
            let server_config_lock = state.server_config.read().expect("lock is poisoned");
            config_for_server(msg.guild_id.unwrap(), &*server_config_lock).and_then(
                |server_config| {
                    server_config
                        .channel(msg.channel_id)
                        .and_then(|channel_config| channel_config.mirror_channel)
                },
            )
        };

        macro_rules! video_too_large {
            () => {
                let res = if let Some(mirrored_channel_id) = mirrored_channel {
                    let res = state
                        .http
                        .create_message(mirrored_channel_id)
                        .content(&format!(
                            "Posted by <@{}> at https://discord.com/channels/{}/{}/{}. Could upload video for {:?} due to file size constraints. You can download it yourself here: {}. Uploading only audio.",
                            msg.author.id,
                            msg.guild_id.unwrap(),
                            msg.channel_id,
                            msg.id,
                            clip_url,
                            clip.2
                        ))?
                        .attachments(&[audio_attachment.clone()])?
                        .exec()
                        .await;

                    if res.is_err() {
                        res
                    } else {


                    let mirror_message: Message = res.unwrap().model().await?;

                    state
                        .http
                        .create_message(msg.channel_id)
                        .reply(msg.id)
                        .content(&format!(
                            "Audio mirror can be found at https://discord.com/channels/{}/{}/{}",
                            msg.guild_id.unwrap(),
                            mirror_message.channel_id,
                            mirror_message.id,
                        ))?
                        .exec()
                        .await
                    }
                } else {
                    state
                        .http
                        .create_message(msg.channel_id)
                        .reply(msg.id)
                        .content(&format!("Could not mirror {:?} due to file size constraints. You can download it yourself here: {}. Uploading only audio.", clip_url, clip.2))?
                        .attachments(&[audio_attachment])?
                        .exec()
                        .await
                };

                if res.is_err() {
                    state
                        .http
                        .create_message(msg.channel_id)
                        .reply(msg.id)
                        .content(&format!("Could not upload just audio for {:?} :(", clip_url))?
                        .exec()
                    .await?;
                }
            }
        }

        if video_too_large {
            video_too_large!();
        } else {
            let res = if let Some(mirrored_channel_id) = mirrored_channel {
                let res = state
                    .http
                    .create_message(mirrored_channel_id)
                    .content(&format!(
                        "Posted by <@{}> at https://discord.com/channels/{}/{}/{}",
                        msg.author.id,
                        msg.guild_id.unwrap(),
                        msg.channel_id,
                        msg.id,
                    ))?
                    .attachments(&[video_attachment, audio_attachment.clone()])?
                    .exec()
                    .await?;

                let mirror_message: Message = res.model().await?;

                state
                    .http
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .content(&format!(
                        "Mirror can be found at https://discord.com/channels/{}/{}/{}",
                        msg.guild_id.unwrap(),
                        mirror_message.channel_id,
                        mirror_message.id,
                    ))?
                    .exec()
                    .await
            } else {
                state
                    .http
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .attachments(&[video_attachment, audio_attachment.clone()])?
                    .exec()
                    .await
            };

            if let Err(e) = &res {
                if let ErrorType::Response {
                    body: _,
                    error,
                    status: _,
                } = e.kind()
                {
                    if let ApiError::General(GeneralApiError { code: 40005, .. }) = error {
                        video_too_large!();
                    }
                } else {
                    res?;
                }
            }
        }

        info!("Message sent!");
    }

    Ok(())
}

async fn extract_audio(data: &[u8]) -> Vec<u8> {
    info!("Extracting audio");
    let mut child = tokio::process::Command::new("ffmpeg")
        .arg("-i")
        .arg("-")
        .arg("-f")
        .arg("mp3")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn ffmpeg");
    let mut stdin = child.stdin.take().expect("Failed to open stdin");

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        info!("Waiting for output");
        let output = child
            .wait_with_output()
            .await
            .expect("Failed to read stdout");
        tx.send(output.stdout)
            .await
            .expect("failed to send tx data");
    });

    info!("Writing input mp4");
    stdin
        .write_all(data)
        .await
        .expect("failed to write mp4 to ffmpeg");
    drop(stdin);
    info!("mp4 written");

    rx.recv().await.expect("no mp3 data sent back?")
}

fn is_command(msg: &Message) -> bool {
    msg.content.starts_with("!")
}

fn config_for_server(server_id: Id<GuildMarker>, config: &Config) -> Option<&ServerConfig> {
    config.servers.iter().find(|server| server.id == server_id)
}

/// Returns the server config corresponding to the given `server_id`. This will
/// create the config if it does not exist.
fn config_for_server_mut(server_id: Id<GuildMarker>, config: &mut Config) -> &mut ServerConfig {
    if let Some(position) = config
        .servers
        .iter()
        .position(|server| server.id == server_id)
    {
        return &mut config.servers[position];
    }

    let server_config = ServerConfig {
        id: server_id,
        monitored_channels: vec![],
    };

    config.servers.push(server_config);

    config.servers.last_mut().unwrap()
}

async fn user_is_admin(message: &Message, state: State) -> anyhow::Result<bool> {
    let get_roles = state.http.roles(message.guild_id.unwrap()).exec().await?;
    let roles: Vec<Role> = get_roles.models().await?;

    for role in roles {
        if role.permissions.contains(Permissions::ADMINISTRATOR) {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn handle_command(msg: Message, state: State) -> anyhow::Result<()> {
    let mut msg_parts = msg.content[1..].split_ascii_whitespace();
    let guild_id = msg.guild_id.expect("message has no guild association?");

    macro_rules! permission_check {
        () => {
            // Ensure that the person executing this command is an admin
            if !user_is_admin(&msg, Arc::clone(&state)).await? {
                info!("User is not an admin -- ignoring command");
                return Ok(());
            }
        };
    }

    match msg_parts.next().unwrap() {
        "watch_channel" => {
            permission_check!();

            let channel_name = msg_parts.next();
            let channel_id = match channel_name {
                Some(mut channel_name) => {
                    if channel_name.starts_with('#') {
                        channel_name = &channel_name[1..];
                    }

                    let channel_list = state.http.guild_channels(guild_id).exec().await?;
                    let channels: Vec<Channel> = channel_list.models().await?;
                    let channel_id = channels.iter().find_map(|channel| {
                        if let Some(enumerated_name) = &channel.name && enumerated_name == channel_name {
                            Some(channel.id)
                        } else {
                            None
                        }
                    });

                    match channel_id {
                        Some(channel_id) => channel_id,
                        None => {
                            state
                                .http
                                .create_message(msg.channel_id)
                                .reply(msg.id)
                                .content(&format!(
                                    "No channel found by the name of {:?}",
                                    channel_name
                                ))?
                                .exec()
                                .await?;

                            return Ok(());
                        }
                    }
                }
                None => msg.channel_id,
            };

            // Explicit new scope so that we don't hold a lock past an `await`
            {
                let mut config_lock = state.server_config.write().unwrap();
                let server_config = config_for_server_mut(guild_id, &mut config_lock);
                server_config.get_or_create_channel(channel_id);

                write_config(&config_lock);
            }

            if let Some(channel_name) = channel_name {
                state
                    .http
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .content(&format!(
                        "Watching channel #{} for Twitch clips",
                        channel_name
                    ))?
                    .exec()
                    .await?;
            } else {
                state
                    .http
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .content("Watching current channel for Twitch clips")?
                    .exec()
                    .await?;
            }

            Ok(())
        }
        "mirror_channel" => {
            permission_check!();

            let channel_name = msg_parts.next();
            let (channel_id, channel_name) = match channel_name {
                Some(mut channel_id) => {
                    if channel_id.starts_with("<#") && channel_id.ends_with('>') {
                        channel_id = channel_id.strip_prefix("<#").unwrap();
                        channel_id = channel_id.strip_suffix('>').unwrap();
                    }

                    let channel_id = channel_id
                        .parse::<u64>()
                        .expect("failed to convert the channel ID to an integer");

                    let channel_list = state.http.guild_channels(guild_id).exec().await?;
                    let channels: Vec<Channel> = channel_list.models().await?;
                    let channel_info = channels.iter().find_map(|channel| {
                        if channel.id == channel_id {
                            Some((channel.id, channel.name.clone()))
                        } else {
                            None
                        }
                    });

                    match channel_info {
                        Some(metadata) => metadata,
                        None => {
                            return Ok(());
                        }
                    }
                }
                None => {
                    state
                        .http
                        .create_message(msg.channel_id)
                        .reply(msg.id)
                        .content("No channel name specified. Usage: !mirror_channel #channel_name")?
                        .exec()
                        .await?;

                    return Ok(());
                }
            };

            // Explicit new scope so that we don't hold a lock past an `await`
            {
                let mut config_lock = state.server_config.write().unwrap();
                let server_config = config_for_server_mut(guild_id, &mut config_lock);
                let channel_config = server_config.get_or_create_channel(msg.channel_id);
                channel_config.mirror_channel = Some(channel_id);

                write_config(&config_lock);
            }

            if let Some(channel_name) = channel_name {
                state
                    .http
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .content(&format!(
                        "Mirrored Twitch clips will now be posted in #{}",
                        channel_name
                    ))?
                    .exec()
                    .await?;
            }

            Ok(())
        }
        "mirror" => {
            if let Some(referenced_message) = msg.referenced_message {
                mirror(*referenced_message, state).await?;
            } else {
                state
                    .http
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .content("Use the !mirror command in reply to a message with a Twitch link")?
                    .exec()
                    .await?;
            }
            Ok(())
        }
        "delete" => {
            permission_check!();

            if let Some(referenced_message) = msg.referenced_message {
                if referenced_message.author.id == state.my_id.load(Ordering::Relaxed) {
                    state
                        .http
                        .delete_message(referenced_message.channel_id, referenced_message.id)
                        .exec()
                        .await?;
                }
            } else {
                let res = state
                    .http
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .content("Use the !mirror command in reply to a message with a Twitch link")?
                    .exec()
                    .await?;
            }

            Ok(())
        }
        other => {
            warn!("command {:?} is not handled by our bot", other);
            Ok(())
        }
    }
}
