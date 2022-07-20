#![feature(let_chains)]

use futures_util::StreamExt;
use hyper::client::{Client as HyperClient, HttpConnector};

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
            }),
        )
    };

    // TODO: Channels get out of sync if they're renamed or and possibly if new permissions are
    // granted.
    let channels = Arc::new(RwLock::new(HashMap::new()));
    let mut my_id = None;

    let _clip_regex = regex::Regex::new(r"https://clips.twitch.tv/[^\s]+");
    while let Some(event) = events.next().await {
        state.standby.process(&event);

        match event {
            Event::Ready(ready) => {
                for guild in ready.guilds {
                    spawn(update_channels(
                        guild.id,
                        channels.clone(),
                        Arc::clone(&state),
                    ));
                }
                my_id = Some(ready.user.id);
                continue;
            }
            Event::MessageCreate(msg) => {
                if msg.guild_id.is_none() || msg.author.bot || msg.author.id == my_id.unwrap() {
                    continue;
                }

                let cached_channels = channels.read().expect("lock poisoned");
                if let Some(channel_name) = cached_channels
                    .get(msg.guild_id.as_ref().unwrap())
                    .map(|guild_channels| guild_channels.get(&msg.channel_id))
                    .flatten()
                {
                    if channel_name != "new-twitch-clips-for-soundboard" {
                        continue;
                    }
                } else {
                    // we don't know about this channel?
                    continue;
                }

                let clip_regex =
                    regex::Regex::new(r#"https://(m\.)?clips\.twitch\.tv/[^\s"']+"#).unwrap();
                if clip_regex.is_match(&msg.content) {
                    spawn(mirror(msg.0, Arc::clone(&state)));
                    continue;
                }
            }
            Event::MemberUpdate(update) => {
                spawn(update_channels(
                    update.guild_id,
                    channels.clone(),
                    Arc::clone(&state),
                ));
                continue;
            }
            _ => {
                // do not care
            }
        }
    }

    Ok(())
}

async fn update_channels(
    id: Id<GuildMarker>,
    cached_channels: Arc<RwLock<HashMap<Id<GuildMarker>, HashMap<Id<ChannelMarker>, String>>>>,
    state: State,
) -> anyhow::Result<()> {
    let channel_list = state.http.guild_channels(id).exec().await?;
    let channels: Vec<Channel> = channel_list.models().await?;
    let mut cached_channels = cached_channels.write().expect("lock poisoned");
    for channel in channels {
        cached_channels
            .entry(id)
            .or_insert(HashMap::default())
            .insert(channel.id, channel.name.expect("channel has no name"));
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
        macro_rules! video_too_large {
            () => {
                    let res = state
                        .http
                        .create_message(msg.channel_id)
                        .reply(msg.id)
                        .content(&format!("Could not mirror {:?} due to file size constraints. You can download it yourself here: {}. Uploading only audio.", clip_url, clip.2))?
                        .attachments(&[audio_attachment])?
                        .exec()
                        .await;

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
            let res = state
                .http
                .create_message(msg.channel_id)
                .reply(msg.id)
                .attachments(&[video_attachment, audio_attachment.clone()])?
                .exec()
                .await;
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
