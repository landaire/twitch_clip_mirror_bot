use futures_util::StreamExt;
use hyper::{
    client::{Client as HyperClient, HttpConnector},
};
use std::{sync::mpsc};
use std::{
    collections::HashMap,
    env,
    future::Future,
    io::{Write},
    process::{Command, Stdio},
    sync::{Arc, RwLock},
};
use twilight_gateway::{Event, Intents, Shard};
use twilight_http::Client as HttpClient;
use twilight_model::{
    channel::{Channel, Message},
    http::attachment::Attachment,
    id::{
        marker::{ChannelMarker, GuildMarker}, Id,
    },
};
use twilight_standby::Standby;

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
            tracing::debug!("handler error: {why:?}");
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize the tracing subscriber.
    tracing_subscriber::fmt::init();

    let (mut events, state) = {
        let token = env::var("DISCORD_TOKEN")?;
        let http = HttpClient::new(token.clone());

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

    //let channels = state.http.guild_channels()
    let channels = Arc::new(RwLock::new(HashMap::new()));

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
                continue;
            }
            Event::MessageCreate(msg) => {
                if msg.guild_id.is_none() {
                    continue;
                }

                let cached_channels = channels.read().expect("lock poisoned");
                if let Some(channel_name) = cached_channels
                    .get(msg.guild_id.as_ref().unwrap())
                    .map(|guild_channels| guild_channels.get(&msg.channel_id))
                    .flatten()
                {
                    if channel_name != "sound-clips" {
                        continue;
                    }
                } else {
                    // we don't know about this channel?
                    continue;
                }

                let clip_regex = regex::Regex::new(r"https://clips.twitch.tv/([^\s]+)").unwrap();
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
    let clip_regex = regex::Regex::new(r"https://(m\.)?clips\.twitch\.tv/[^\s]+").unwrap();

    for cap in clip_regex.captures_iter(&msg.content) {
        let clip_url = &cap[0];

        let clip = twitch::download_clip(clip_url).await;
        if clip.is_none() {
            state
                .http
                .create_message(msg.channel_id)
                .content(&format!("Could not download {:?}", clip_url))?
                .exec()
                .await?;
            continue;
        }

        let clip = clip.unwrap();
        let audio_attachment = Attachment::from_bytes(
            format!("{}.mp3", &clip.0),
            extract_audio(clip.1.as_slice()),
            2,
        );
        let video_attachment = Attachment::from_bytes(format!("{}.mp4", &clip.0), clip.1, 1);

        state
            .http
            .create_message(msg.channel_id)
            .reply(msg.id)
            .attachments(&[video_attachment, audio_attachment])?
            .exec()
            .await?;
    }

    Ok(())
}

fn extract_audio(data: &[u8]) -> Vec<u8> {
    let mut child = Command::new("ffmpeg")
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


    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let output = child.wait_with_output().expect("Failed to read stdout");
        tx.send(output.stdout).expect("failed to send tx data");
    });

    stdin.write_all(data).expect("failed to write mp4 to ffmpeg");
    drop(stdin);


    rx.recv().expect("no mp3 data sent back?")
}
