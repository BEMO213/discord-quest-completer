pub use discord_sdk as ds;
pub use tokio;

use std::time::Duration;

pub struct Client {
    pub discord: ds::Discord,
    pub wheel: ds::wheel::Wheel,
    pub user: ds::user::User,
}

pub async fn make_client(app_id: ds::AppId, subs: ds::Subscriptions) -> Result<Client, String> {
    println!("Creating Discord client with app ID: {}", app_id);
    let (wheel, handler) = ds::wheel::Wheel::new(Box::new(|err| {
        eprintln!("Discord SDK error: {:?}", err);
    }));

    let mut user = wheel.user();
    let discord = ds::Discord::new(ds::DiscordApp::PlainId(app_id), subs, Box::new(handler))
        .map_err(|e| format!("Unable to create Discord client: {:?}", e))?;

    tokio::time::timeout(Duration::from_secs(20), user.0.changed())
        .await
        .map_err(|_| "Timed out while connecting to Discord".to_string())?
        .map_err(|e| format!("Discord connection listener failed: {:?}", e))?;

    let connected_user = {
        let state = user.0.borrow();
        match &*state {
            ds::wheel::UserState::Connected(user) => user.clone(),
            ds::wheel::UserState::Disconnected(err) => {
                return Err(format!("Failed to connect to Discord: {:?}", err));
            }
        }
    };

    println!("Connected to Discord, local user is {:#?}", connected_user);
    Ok(Client {
        discord,
        wheel,
        user: connected_user,
    })
}
