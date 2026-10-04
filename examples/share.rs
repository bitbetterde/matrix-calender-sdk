//! Creates calendars and invites a user to them, for trying the receiving side of sharing in
//! a client.
//!
//! Each argument is one calendar as `name[:colour[:role]]`, where `colour` is `#rrggbb`
//! (default `#3e63dd`) and `role` is `reader` (default) or `editor`. A calendar with that name
//! is reused if the account already has one.
//!
//! ```sh
//! MATRIX_HOMESERVER=http://matrix.ess.localhost:8008 \
//! MATRIX_USER=alice MATRIX_PASSWORD=calendar-e2e-alice \
//! MATRIX_SHARE_WITH=@demo:ess.localhost \
//! cargo run --example share -- "Team:#e5484d:editor" "Holidays:#30a46c"
//! ```
//!
//! Every run logs in as a new device with a fresh SQLite store in the system's temp directory;
//! the device is logged out and the store deleted at the end.

use std::{
    env,
    error::Error,
    time::{Duration, SystemTime},
};

use matrix_calendar_sdk::{
    calendar::{Calendar, CreateCalendarOptions, list_calendars},
    events::CalendarInfo,
    permissions::CalendarRole,
};
use matrix_sdk::{Client, config::SyncSettings, ruma::OwnedUserId};

const DEFAULT_COLOR: &str = "#3e63dd";

struct Spec {
    name: String,
    color: String,
    role: CalendarRole,
}

fn parse_spec(arg: &str) -> Result<Spec, Box<dyn Error>> {
    let mut parts = arg.splitn(3, ':');
    let name = parts.next().unwrap_or_default().trim();
    if name.is_empty() {
        return Err(format!("empty calendar name in {arg:?}").into());
    }
    let color = match parts.next().map(str::trim) {
        Some(color) if !color.is_empty() => color.to_owned(),
        _ => DEFAULT_COLOR.to_owned(),
    };
    let role = match parts.next().map(str::trim) {
        None | Some("") | Some("reader") => CalendarRole::Reader,
        Some("editor") => CalendarRole::Editor,
        Some(other) => return Err(format!("role must be reader or editor, not {other}").into()),
    };
    Ok(Spec {
        name: name.to_owned(),
        color,
        role,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let specs = env::args()
        .skip(1)
        .map(|arg| parse_spec(&arg))
        .collect::<Result<Vec<_>, _>>()?;
    if specs.is_empty() {
        return Err("pass at least one calendar as name[:colour[:role]]".into());
    }
    let share_with: OwnedUserId = env::var("MATRIX_SHARE_WITH")?.parse()?;

    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_millis();
    let store_path = env::temp_dir().join(format!(
        "matrix-calendar-sdk-share-{}-{nonce}",
        std::process::id()
    ));
    let (client, result) = match login(&store_path).await {
        Ok(client) => {
            let result = run(&client, &specs, &share_with).await;
            (Some(client), result)
        }
        Err(err) => (None, Err(err)),
    };

    if let Some(client) = client
        && let Err(err) = client.logout().await
    {
        eprintln!("Logging out failed: {err}");
    }
    if let Err(err) = std::fs::remove_dir_all(&store_path) {
        eprintln!("Removing {} failed: {err}", store_path.display());
    }
    result
}

async fn login(store_path: &std::path::Path) -> Result<Client, Box<dyn Error>> {
    let homeserver = env::var("MATRIX_HOMESERVER")?;
    let user = env::var("MATRIX_USER")?;
    let password = env::var("MATRIX_PASSWORD")?;
    let client = Client::builder()
        .server_name_or_homeserver_url(&homeserver)
        .sqlite_store(store_path, None)
        .build()
        .await?;
    client
        .matrix_auth()
        .login_username(&user, &password)
        .initial_device_display_name("matrix-calendar-sdk share example")
        .send()
        .await?;
    Ok(client)
}

async fn run(
    client: &Client,
    specs: &[Spec],
    share_with: &OwnedUserId,
) -> Result<(), Box<dyn Error>> {
    client
        .sync_once(SyncSettings::default().timeout(Duration::from_secs(5)))
        .await?;
    let existing = list_calendars(client);

    for spec in specs {
        let found = existing
            .iter()
            .find(|calendar| calendar.room().name().as_deref() == Some(spec.name.as_str()));
        let calendar = match found {
            Some(calendar) => {
                println!("Found calendar {} ({})", spec.name, calendar.room_id());
                calendar.clone()
            }
            None => {
                let mut options = CreateCalendarOptions::new(&spec.name);
                options.info = CalendarInfo::new(Some(spec.color.clone()), None)?;
                let calendar = Calendar::create(client, options).await?;
                println!("Created calendar {} ({})", spec.name, calendar.room_id());
                calendar
            }
        };
        calendar.invite(share_with, spec.role).await?;
        println!("Invited {share_with} to {} as {:?}", spec.name, spec.role);
    }
    Ok(())
}
