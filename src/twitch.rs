use once_cell::sync::OnceCell;
use regex::Regex;
use std::env;
use std::sync::Arc;
use twitch_api_rs::auth::client_credentials::{ClientAuthRequest, ClientAuthToken};
use twitch_api_rs::auth::{ClientId, ClientSecret};
use twitch_api_rs::prelude::*;
use twitch_api_rs::resource::clips::get_clips::*;

static REQWEST_CLIENT: OnceCell<Arc<reqwest::Client>> = OnceCell::new();

fn get_client() -> Arc<reqwest::Client> {
    REQWEST_CLIENT
        .get_or_init(|| Arc::new(reqwest::Client::new()))
        .clone()
}

async fn get_auth_token() -> Arc<ClientAuthToken> {
    let client_id: ClientId = env::var("TWITCH_CLIENT_ID")
        .expect("expected TWITCH_CLIENT_ID env var")
        .into();
    let client_secret: ClientSecret = env::var("TWITCH_CLIENT_SECRET")
        .expect("expected TWITCH_CLIENT_SECRET env var")
        .into();
    // Get a client credentials (application) access token and wrap it in an arc
    // to be used across multiple tasks without repeating
    let auth_token: Arc<ClientAuthToken> = Arc::new(
        match ClientAuthRequest::builder()
            .set_client_id(client_id.clone())
            .set_client_secret(client_secret)
            .make_request(get_client())
            .await
        {
            Ok(resp) => {
                // Create the token from the token value provided by twitch and
                // your client_id
                ClientAuthToken::from_client(resp, client_id)
            }

            // Better error handling can be performed here by matching against
            // the type of this requests::RequestError. Elided for conciseness
            Err(e) => panic!("Could not complete auth request for reason {}", e),
        },
    );

    auth_token
}

fn clip_thumbnail_url_to_video_url(thumbnail_url: &str) -> String {
    let re = Regex::new(r"(-social)?-preview.+").unwrap();
    re.replace(thumbnail_url, ".mp4").into_owned()
}

pub async fn download_clip_from_thumbnail_url(thumbnail_url: &str) -> Option<(Vec<u8>, String)> {
    let client = get_client();
    let mp4_url = clip_thumbnail_url_to_video_url(thumbnail_url);
    match client.get(&mp4_url).send().await {
        Ok(resp) => {
            let content = resp
                .bytes()
                .await
                .expect("Failed to get response bytes")
                .to_vec();

            Some((content, mp4_url))
        }
        Err(e) => {
            panic!("{:?}", e);
        }
    }
}

pub async fn download_clip(clip_url: &str) -> Option<(String, Vec<u8>, String)> {
    let clip_url = reqwest::Url::parse(clip_url).expect("invalid URL");

    let clip = match GetClipsRequest::builder()
        .set_auth(get_auth_token().await)
        .add_clip_id(&clip_url.path()[1..])
        .make_request(get_client())
        .await
    {
        Ok(mut resp) => {
            if let Some(clip) = resp.clips.pop() {
                clip
            } else {
                return None;
            }
        }
        Err(e) => panic!("Could not get clips for user for reason {}", e),
    };

    println!("{:?} {:?}", clip.title, clip.thumbnail_url);

    download_clip_from_thumbnail_url(clip.thumbnail_url.as_str())
        .await
        .map(|(content, mp4_url)| (clip.title.as_str().to_owned(), content, mp4_url))
}
