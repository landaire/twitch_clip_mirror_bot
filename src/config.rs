use std::path::{Path, PathBuf};

use log::{info, warn};
use serde::{Deserialize, Serialize};
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker},
    Id,
};

const CONFIG_FILE_PATH: &'static str = "config.toml";

#[derive(Default, Debug, Serialize, Deserialize)]
pub(crate) struct Config {
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ServerConfig {
    /// This server's ID
    pub id: Id<GuildMarker>,
    /// Per-channel config
    pub monitored_channels: Vec<Channel>,
}

impl ServerConfig {
    pub fn get_or_create_channel(&mut self, channel_id: Id<ChannelMarker>) -> &mut Channel {
        if let Some(position) = self
            .monitored_channels
            .iter()
            .position(|channel| channel.id == channel_id)
        {
            return &mut self.monitored_channels[position];
        }

        let channel = Channel {
            id: channel_id,
            mirror_channel: None,
        };

        self.monitored_channels.push(channel);

        self.monitored_channels.last_mut().unwrap()
    }

    pub fn channel(&self, channel_id: Id<ChannelMarker>) -> Option<&Channel> {
        if let Some(position) = self
            .monitored_channels
            .iter()
            .position(|channel| channel.id == channel_id)
        {
            return Some(&self.monitored_channels[position]);
        }

        None
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Channel {
    /// This channel's ID
    pub id: Id<ChannelMarker>,
    /// If this field is not present, post responses inline in
    /// the channel that originally had the Twitch clip link
    pub mirror_channel: Option<Id<ChannelMarker>>,
}

pub(crate) fn load_config() -> Config {
    let config_path = PathBuf::from(CONFIG_FILE_PATH);
    if !config_path.exists() {
        // config doesn't exist, just return an empty list
        info!("config file does not exist");
        return Config::default();
    }

    let config_contents = std::fs::read(config_path);
    if config_contents.is_err() {
        warn!("could not read config file");
        return Config::default();
    }

    let config_contents = config_contents.unwrap();

    let config: Config =
        toml::from_slice(config_contents.as_slice()).unwrap_or_else(|_| Config::default());

    config
}

pub(crate) fn write_config(config: &Config) {
    std::fs::write(
        CONFIG_FILE_PATH,
        toml::to_vec(config).expect("failed to serialize ServerConfig"),
    )
    .expect("failed to write server config")
}
