//! A small end-to-end example against a real homeserver.
//!
//! It logs in, finds or creates a calendar room named "Example calendar", finds or adds a
//! weekly event ("Weekly planning", Mondays at 09:00 Europe/Berlin), prints the occurrences of
//! the next 30 days and, if `MATRIX_SHARE_WITH` is set, shares the calendar with that user
//! and sends them an invite in a direct-message room. Running it again reuses the calendar and
//! the event.
//!
//! The calendar is shared **read-only** by default; set `MATRIX_SHARE_ROLE=editor` to let the
//! other user create events too (`reader` is the default). Sharing again with another role
//! changes the user's role. The homeserver enforces the role through power levels.
//!
//! ```sh
//! MATRIX_HOMESERVER=https://matrix.example.org \
//! MATRIX_USER=@alice:example.org \
//! MATRIX_PASSWORD=secret \
//! MATRIX_SHARE_WITH=@bob:example.org \
//! MATRIX_SHARE_ROLE=reader \
//! cargo run --example basic
//! ```
//!
//! Every run logs in as a new device with a fresh SQLite store in the system's temp directory;
//! the device is logged out and the store deleted at the end.

use std::{
    env,
    error::Error,
    time::{Duration, SystemTime},
};

use chrono::{Datelike, Days, TimeDelta, Utc};
use chrono_tz::{Europe::Berlin, Tz};
use matrix_calendar_sdk::{
    calendar::{Calendar, CreateCalendarOptions, list_calendars},
    events::{
        ByDay, CalendarEvent, CalendarInfo, CalendarInvite, EventTime, Frequency, RRule,
        Recurrence, Weekday,
    },
    permissions::{CalendarPermissions, CalendarRole},
    scheduling,
};
use matrix_sdk::{Client, config::SyncSettings, ruma::OwnedUserId};

const CALENDAR_NAME: &str = "Example calendar";
const EVENT_TITLE: &str = "Weekly planning";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_millis();
    let store_path = env::temp_dir().join(format!(
        "matrix-calendar-sdk-example-{}-{nonce}",
        std::process::id()
    ));
    let (client, result) = match login(&store_path).await {
        Ok(client) => {
            let result = run(&client).await;
            (Some(client), result)
        }
        Err(err) => (None, Err(err)),
    };

    // Clean up even if the example failed: log the device out and delete the store.
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

/// Logs in as a new device with a fresh SQLite store at `store_path`.
async fn login(store_path: &std::path::Path) -> Result<Client, Box<dyn Error>> {
    let homeserver = env::var("MATRIX_HOMESERVER")?;
    let user = env::var("MATRIX_USER")?;
    let password = env::var("MATRIX_PASSWORD")?;

    println!("Using store at {}", store_path.display());
    let client = Client::builder()
        .server_name_or_homeserver_url(&homeserver)
        .sqlite_store(store_path, None)
        .build()
        .await?;
    client
        .matrix_auth()
        .login_username(&user, &password)
        .initial_device_display_name("matrix-calendar-sdk example")
        .send()
        .await?;
    Ok(client)
}

async fn run(client: &Client) -> Result<(), Box<dyn Error>> {
    let share_with: Option<OwnedUserId> = match env::var("MATRIX_SHARE_WITH") {
        Ok(user_id) => Some(user_id.parse()?),
        Err(_) => None,
    };
    let share_role = match env::var("MATRIX_SHARE_ROLE").as_deref() {
        Ok("editor") => CalendarRole::Editor,
        Ok("reader") | Err(_) => CalendarRole::Reader,
        Ok(other) => {
            return Err(format!("MATRIX_SHARE_ROLE must be reader or editor, not {other}").into());
        }
    };
    client.sync_once(SyncSettings::default()).await?;

    // Find or create the calendar room.
    let existing = list_calendars(client)
        .into_iter()
        .find(|calendar| calendar.room().name().as_deref() == Some(CALENDAR_NAME));
    let calendar = match existing {
        Some(calendar) => {
            println!("Found calendar {}", calendar.room_id());
            calendar
        }
        None => {
            let mut opts = CreateCalendarOptions::new(CALENDAR_NAME);
            opts.info = CalendarInfo::new(Some("#3366cc".into()), Some(Berlin))?;
            let calendar = Calendar::create(client, opts).await?;
            println!("Created calendar {}", calendar.room_id());
            calendar
        }
    };

    // A weekly event every Monday 09:00–10:00 Berlin time: reuse the one an earlier run created,
    // or create it starting next Monday.
    let own_user_id = client.user_id().ok_or("not logged in")?.to_owned();
    let index = calendar.load().await?;
    let existing = index.iter().find(|entry| {
        entry.sender == own_user_id
            && entry.content.title == EVENT_TITLE
            && entry.content.is_recurring()
            && !entry.content.is_cancelled()
    });
    let event = match existing {
        Some(entry) => {
            println!("Found event {} (uid {})", entry.event_id, entry.key.uid);
            entry.content.clone()
        }
        None => {
            let today = Utc::now().with_timezone(&Berlin).date_naive();
            let days_until_monday = 7 - u64::from(today.weekday().num_days_from_monday());
            let monday = today + Days::new(days_until_monday);
            let start_local = monday.and_hms_opt(9, 0, 0).ok_or("invalid start time")?;
            // The organiser is optional (a missing one means the sender); naming it
            // explicitly keeps invites self-describing for bridges.
            let mut event = CalendarEvent::new(
                EVENT_TITLE,
                EventTime::timed(start_local, Berlin)?,
                EventTime::timed(start_local + TimeDelta::hours(1), Berlin)?,
                Some(own_user_id.clone()),
            );
            event.description = Some("Created by the matrix-calendar-sdk example.".into());
            let mut rule = RRule::new(Frequency::Weekly);
            rule.byday = Some(vec![ByDay::every(Weekday::Monday)]);
            event.recurrence = Some(Recurrence::new(rule));
            let event_id = calendar.create_event(event.clone()).await?;
            println!("Created event {event_id} (uid {})", event.uid);
            event
        }
    };

    // Sync once more so the room state (calendar_info, timezone) of a new calendar is known
    // locally. A short long-poll timeout: if nothing changed, there is nothing to wait for.
    client
        .sync_once(SyncSettings::default().timeout(Duration::from_secs(2)))
        .await?;

    // Print the occurrences of the next 30 days.
    let now = Utc::now();
    let tz = calendar.default_timezone().await?;
    let occurrences = calendar.occurrences(now, now + TimeDelta::days(30)).await?;
    println!(
        "Occurrences in the next 30 days ({} total):",
        occurrences.len()
    );
    for occurrence in &occurrences {
        let display_tz: Tz = occurrence.occurrence.start.timezone().unwrap_or(tz);
        println!(
            "  {}  {}{}",
            occurrence
                .start_utc(tz)
                .with_timezone(&display_tz)
                .format("%a %Y-%m-%d %H:%M %Z"),
            occurrence.occurrence.event.title,
            if occurrence.occurrence.is_override {
                " (changed)"
            } else {
                ""
            },
        );
    }

    // Share the calendar and send an invite in a DM.
    if let Some(user_id) = share_with {
        // Calendars created before the SDK managed permissions (or by other clients) have
        // Matrix's default power levels, where every member can write. Bring them under the
        // SDK's scheme first, keeping write access for existing members (default role editor).
        if !calendar.permissions().await?.is_standard {
            let mut permissions = CalendarPermissions::new(CalendarRole::Editor);
            permissions.readers_can_rsvp = true;
            calendar.apply_permissions(&permissions).await?;
            println!("Applied the SDK's permission scheme (default role: editor)");
        }
        // Sets the role first, then invites; for a user who is already a member it only
        // adjusts the role.
        calendar.invite(&user_id, share_role).await?;
        println!("Shared the calendar with {user_id} as {share_role}");

        let dm = match client.get_dm_room(&user_id) {
            Some(room) => room,
            None => client.create_dm(&user_id).await?,
        };
        let invite_id = scheduling::send_invite(&dm, CalendarInvite::request(&event, 0)).await?;
        println!(
            "Sent calendar invite {invite_id} to {user_id} in {}",
            dm.room_id()
        );
    }

    // Who has access, and with which role. Roles are read from the locally synced room state,
    // so sync once to see the changes made above.
    client
        .sync_once(SyncSettings::default().timeout(Duration::from_secs(2)))
        .await?;
    println!("Members:");
    for member in calendar.members_with_roles().await? {
        println!(
            "  {:<8} {:?} {}",
            member.role.to_string(),
            member.membership,
            member.user_id
        );
    }

    Ok(())
}
