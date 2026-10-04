//! End-to-end tests against a real homeserver.
//!
//! These tests only run when `MATRIX_E2E_HOMESERVER` is set (for example
//! `http://matrix.ess.localhost`); otherwise every test prints "skipped" and passes, so that
//! `cargo test` stays green without a server. See `scripts/e2e.sh` and the README.
//!
//! The server must have the users `alice`, `bob` and `mallory`, whose passwords are
//! `$MATRIX_E2E_PASSWORD_PREFIX<localpart>` (the prefix defaults to `calendar-e2e-`).
//!
//! Every test logs in fresh devices (in-memory stores), creates its own uniquely named rooms,
//! and leaves those rooms and logs its devices out at the end (also when it fails), so tests
//! are independent of each other and of data left on the server by earlier runs.

// `Client::sync` futures are deeply nested; spawning one needs a higher limit.
#![recursion_limit = "256"]

use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use chrono::{DateTime, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::{Europe::Berlin, Tz};
use matrix_calendar_sdk::{
    Error,
    availability::{
        evaluate_tier, get_policy, get_window, query_availability, set_policy, set_window,
        unavailable_from_windows,
    },
    calendar::{
        Calendar, CalendarIndex, CalendarInvitation, CalendarOccurrence, CreateCalendarOptions,
        EventKey, LoadOptions, list_calendar_invitations, list_calendars,
    },
    events::{
        AvailabilityPolicyEventContent, AvailabilityWindowEventContent, BusyInterval, BusyType,
        ByDay, CalendarEvent, CalendarEventEventContent, CalendarInfo, CalendarInfoEventContent,
        CalendarInvite, CalendarInviteEventContent, EventTime, Frequency, Grant, InviteMethod,
        Location, PartStat, RRule, Recurrence, Status, Transparency, Weekday, Window,
    },
    ids,
    permissions::{CalendarMembership, CalendarPermissions, CalendarRole},
    scheduling::{self, SchedulingIndex},
};
use matrix_sdk::{
    Client, Room, RoomState,
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
        api::client::{
            membership::{forget_room, leave_room},
            room::{create_room, get_room_event},
            state::get_state_events,
        },
        api::error::ErrorKind,
        events::{AnyTimelineEvent, room::member::MembershipState},
        serde::Raw,
    },
};
use serde_json::{Value, json};
use tokio::{sync::broadcast, task::JoinSet};

/// How long any single wait may take before the test fails.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for a whole test, including teardown of a failing test.
const TEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Long-poll timeout of the `/sync` requests made by [`TestUser::sync_until`].
const SYNC_POLL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// Configuration from the environment.
#[derive(Clone, Debug)]
struct Env {
    homeserver: String,
    password_prefix: String,
}

impl Env {
    fn from_env() -> Option<Self> {
        let homeserver = std::env::var("MATRIX_E2E_HOMESERVER")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        let password_prefix = std::env::var("MATRIX_E2E_PASSWORD_PREFIX")
            .unwrap_or_else(|_| "calendar-e2e-".to_owned());
        Some(Self {
            homeserver,
            password_prefix,
        })
    }
}

/// Per-test context: logs users in and remembers what to clean up.
#[derive(Clone)]
struct Ctx {
    inner: Arc<CtxInner>,
}

struct CtxInner {
    env: Env,
    test: &'static str,
    run_id: String,
    started: Instant,
    clients: Mutex<Vec<Client>>,
    rooms: Mutex<Vec<OwnedRoomId>>,
    syncs: Mutex<Vec<tokio::task::AbortHandle>>,
}

/// Runs the test `body` if `MATRIX_E2E_HOMESERVER` is set, then cleans up (leaves the test's
/// rooms, logs out its devices) whether the body passed or panicked, and re-raises a panic.
async fn run<F, Fut>(test: &'static str, body: F)
where
    F: FnOnce(Ctx) -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(env) = Env::from_env() else {
        eprintln!("skipped {test}: set MATRIX_E2E_HOMESERVER to run the end-to-end tests");
        return;
    };
    let ctx = Ctx {
        inner: Arc::new(CtxInner {
            env,
            test,
            run_id: uuid::Uuid::new_v4().simple().to_string()[..12].to_owned(),
            started: Instant::now(),
            clients: Mutex::new(Vec::new()),
            rooms: Mutex::new(Vec::new()),
            syncs: Mutex::new(Vec::new()),
        }),
    };
    ctx.log("start");
    let body = body(ctx.clone());
    let result = CatchUnwind(Box::pin(async {
        tokio::time::timeout(TEST_TIMEOUT, body)
            .await
            .unwrap_or_else(|_| panic!("{test} did not finish within {TEST_TIMEOUT:?}"));
    }))
    .await;
    let elapsed = ctx.inner.started.elapsed();
    ctx.teardown().await;
    match result {
        Ok(()) => ctx.log(&format!("passed in {:.1}s", elapsed.as_secs_f64())),
        Err(panic) => {
            ctx.log(&format!("FAILED after {:.1}s", elapsed.as_secs_f64()));
            resume_unwind(panic);
        }
    }
}

/// A future that turns a panic while polling the inner future into an `Err`, so that the
/// test can clean up before re-raising it (the test body is not `Send`, so it cannot simply be
/// spawned and joined).
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = std::thread::Result<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}

impl Ctx {
    fn log(&self, msg: &str) {
        eprintln!(
            "[{} +{:.1}s] {msg}",
            self.inner.test,
            self.inner.started.elapsed().as_secs_f64()
        );
    }

    /// A name that is unique to this test run.
    fn unique(&self, label: &str) -> String {
        format!("e2e {} {label} {}", self.inner.test, self.inner.run_id)
    }

    /// A uid that is unique to this test run.
    fn uid(&self, label: &str) -> String {
        format!("{}-{label}-{}", self.inner.test, uuid::Uuid::new_v4())
    }

    /// Logs in a fresh device of `localpart` with an in-memory store and syncs once (which
    /// also uploads the device keys).
    async fn login(&self, localpart: &str) -> TestUser {
        let env = &self.inner.env;
        let client = Client::builder()
            .homeserver_url(&env.homeserver)
            .build()
            .await
            .unwrap_or_else(|e| panic!("building a client for {}: {e}", env.homeserver));
        let password = format!("{}{localpart}", env.password_prefix);
        let response = client
            .matrix_auth()
            .login_username(localpart, &password)
            .initial_device_display_name(&format!(
                "matrix-calendar-sdk e2e {} {}",
                self.inner.test, self.inner.run_id
            ))
            .send()
            .await
            .unwrap_or_else(|e| panic!("login as {localpart} failed: {e}"));
        self.inner
            .clients
            .lock()
            .expect("lock")
            .push(client.clone());
        client
            .sync_once(SyncSettings::default().timeout(Duration::ZERO))
            .await
            .unwrap_or_else(|e| panic!("initial sync of {localpart} failed: {e}"));
        self.log(&format!(
            "logged in {} (device {})",
            response.user_id, response.device_id
        ));
        TestUser {
            client,
            user_id: response.user_id,
            ctx: self.clone(),
        }
    }

    /// Remembers `room_id` so that every user of this test leaves it at the end.
    fn track_room(&self, room_id: &RoomId) {
        self.inner
            .rooms
            .lock()
            .expect("lock")
            .push(room_id.to_owned());
    }

    async fn teardown(&self) {
        for handle in self.inner.syncs.lock().expect("lock").drain(..) {
            handle.abort();
        }
        let clients: Vec<Client> = self.inner.clients.lock().expect("lock").drain(..).collect();
        let rooms: Vec<OwnedRoomId> = self.inner.rooms.lock().expect("lock").clone();
        let mut tasks = JoinSet::new();
        for client in clients {
            let rooms = rooms.clone();
            tasks.spawn(async move {
                for room_id in rooms {
                    // Not every user is in every room; errors are expected and ignored.
                    let _ = client
                        .send(leave_room::v3::Request::new(room_id.clone()))
                        .await;
                    let _ = client.send(forget_room::v3::Request::new(room_id)).await;
                }
                let _ = client.logout().await;
            });
        }
        let cleanup = async { while tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(TIMEOUT, cleanup).await.is_err() {
            self.log("teardown timed out; some rooms or devices may be left on the server");
        }
    }
}

/// A logged-in device of a test user.
#[derive(Clone)]
struct TestUser {
    client: Client,
    user_id: OwnedUserId,
    ctx: Ctx,
}

impl TestUser {
    /// Syncs (long-polling) until `cond` holds for the client.
    async fn sync_until(&self, what: &str, mut cond: impl FnMut(&Client) -> bool) {
        let start = Instant::now();
        loop {
            if cond(&self.client) {
                return;
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "{}: timed out after {TIMEOUT:?} waiting for {what}",
                self.user_id
            );
            self.sync_once().await;
        }
    }

    /// Syncs (long-polling) until the async check `f` returns `Some`.
    async fn sync_until_some<T>(&self, what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
        let start = Instant::now();
        loop {
            if let Some(value) = f().await {
                return value;
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "{}: timed out after {TIMEOUT:?} waiting for {what}",
                self.user_id
            );
            self.sync_once().await;
        }
    }

    async fn sync_once(&self) {
        self.client
            .sync_once(SyncSettings::default().timeout(SYNC_POLL))
            .await
            .unwrap_or_else(|e| panic!("{}: sync failed: {e}", self.user_id));
    }

    /// Starts syncing in the background until the end of the test.
    fn start_sync(&self) {
        let client = self.client.clone();
        let user_id = self.user_id.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = client
                .sync(SyncSettings::default().timeout(SYNC_POLL))
                .await
            {
                eprintln!("{user_id}: background sync stopped: {e}");
            }
        });
        self.ctx
            .inner
            .syncs
            .lock()
            .expect("lock")
            .push(handle.abort_handle());
    }

    /// Waits for an invite to `room_id`, joins and syncs until the join has come back through
    /// sync (so that the room state is complete, not just the stripped invite state).
    async fn join(&self, room_id: &RoomId) -> Room {
        self.sync_until(&format!("an invite to {room_id}"), |c| {
            c.get_room(room_id)
                .is_some_and(|r| matches!(r.state(), RoomState::Invited | RoomState::Joined))
        })
        .await;
        self.client
            .join_room_by_id(room_id)
            .await
            .unwrap_or_else(|e| panic!("{} could not join {room_id}: {e}", self.user_id));
        // `join_room_by_id` marks the room as joined locally right away; the room state only
        // arrives with the next sync, together with our own join event.
        self.wait_for_members(room_id, std::slice::from_ref(&self.user_id))
            .await;
        self.room(room_id)
    }

    /// Joins the calendar `room_id` and waits until it is recognised as a calendar.
    async fn join_calendar(&self, room_id: &RoomId) -> Calendar {
        self.join(room_id).await;
        self.sync_until(&format!("{room_id} to be recognised as a calendar"), |c| {
            c.get_room(room_id)
                .is_some_and(|r| Calendar::try_from_room(r).is_ok())
        })
        .await;
        Calendar::try_from_room(self.client.get_room(room_id).expect("room")).expect("calendar")
    }

    /// Syncs until the local room state has a join membership event for each of `users`
    /// (needed before sending encrypted events, so that the room key is shared with them).
    ///
    /// This does not use `Room::joined_members_count`: Synapse only sends the room summary
    /// (`m.joined_member_count`) when members are lazy-loaded, so the count is not updated.
    async fn wait_for_members(&self, room_id: &RoomId, users: &[OwnedUserId]) {
        self.sync_until_some(
            &format!("{users:?} to be joined to {room_id}"),
            async || {
                let room = self.client.get_room(room_id)?;
                for user in users {
                    let member = room.get_member_no_sync(user).await.ok()??;
                    if *member.membership() != MembershipState::Join {
                        return None;
                    }
                }
                Some(())
            },
        )
        .await;
    }

    fn room(&self, room_id: &RoomId) -> Room {
        self.client
            .get_room(room_id)
            .unwrap_or_else(|| panic!("{} does not know {room_id}", self.user_id))
    }
}

/// Creates a calendar owned by `owner` (home timezone Europe/Berlin) with every member invited
/// as an **editor** (members are readers by default), lets every member join and waits until
/// the owner sees all of them joined. Returns the owner's calendar and the members'
/// calendars, in order.
async fn shared_calendar(
    ctx: &Ctx,
    owner: &TestUser,
    members: &[&TestUser],
    encrypted: bool,
) -> (Calendar, Vec<Calendar>) {
    let mut opts = CreateCalendarOptions::new(ctx.unique("calendar"));
    opts.info = CalendarInfo::new(Some("#3366cc".into()), Some(Berlin)).expect("valid info");
    opts.members = members
        .iter()
        .map(|m| (m.user_id.clone(), CalendarRole::Editor))
        .collect();
    opts.encrypted = encrypted;
    let calendar = Calendar::create(&owner.client, opts)
        .await
        .expect("create calendar");
    ctx.track_room(calendar.room_id());
    let mut joined = Vec::new();
    for member in members {
        joined.push(member.join_calendar(calendar.room_id()).await);
    }
    owner
        .wait_for_members(calendar.room_id(), &user_ids(members))
        .await;
    (calendar, joined)
}

fn user_ids(users: &[&TestUser]) -> Vec<OwnedUserId> {
    users.iter().map(|u| u.user_id.clone()).collect()
}

/// Creates a plain (non-calendar) room inviting `members`, lets them join and waits until the
/// owner sees everyone joined.
async fn shared_room(ctx: &Ctx, owner: &TestUser, members: &[&TestUser]) -> (Room, Vec<Room>) {
    let mut request = create_room::v3::Request::new();
    request.name = Some(ctx.unique("room"));
    request.invite = members.iter().map(|m| m.user_id.clone()).collect();
    let room = owner
        .client
        .create_room(request)
        .await
        .expect("create room");
    ctx.track_room(room.room_id());
    let mut joined = Vec::new();
    for member in members {
        joined.push(member.join(room.room_id()).await);
    }
    owner
        .wait_for_members(room.room_id(), &user_ids(members))
        .await;
    (room, joined)
}

// ---------------------------------------------------------------------------------------------
// Event helpers
// ---------------------------------------------------------------------------------------------

fn local(s: &str) -> NaiveDateTime {
    s.parse().expect("valid local date-time")
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .expect("valid RFC 3339")
        .with_timezone(&Utc)
}

fn berlin(s: &str) -> EventTime {
    EventTime::timed(local(s), Berlin).expect("valid Berlin time")
}

/// A timed Europe/Berlin event organised by `organizer`.
fn timed_event(title: &str, organizer: &UserId, start: &str, end: &str) -> CalendarEvent {
    CalendarEvent::new(title, berlin(start), berlin(end), organizer.to_owned())
}

/// A weekly Europe/Berlin series on `weekday`.
fn weekly_event(
    title: &str,
    organizer: &UserId,
    start: &str,
    end: &str,
    weekday: Weekday,
) -> CalendarEvent {
    let mut event = timed_event(title, organizer, start, end);
    let mut rule = RRule::new(Frequency::Weekly);
    rule.byday = Some(vec![ByDay::every(weekday)]);
    event.recurrence = Some(Recurrence::new(rule));
    event
}

fn localtime(t: &EventTime) -> NaiveDateTime {
    match t {
        EventTime::Timed { localtime, .. } => *localtime,
        EventTime::AllDay { date } => date.and_time(NaiveTime::MIN),
    }
}

/// `(uid, local start, UTC start)` of each occurrence.
fn summarize(
    occurrences: &[CalendarOccurrence],
    tz: Tz,
) -> Vec<(String, NaiveDateTime, DateTime<Utc>)> {
    occurrences
        .iter()
        .map(|o| {
            (
                o.occurrence.uid.clone(),
                localtime(&o.occurrence.start),
                o.start_utc(tz),
            )
        })
        .collect()
}

/// Loads the calendar until `check` accepts the index.
async fn load_until(
    user: &TestUser,
    calendar: &Calendar,
    what: &str,
    mut check: impl FnMut(&CalendarIndex) -> bool,
) -> CalendarIndex {
    user.sync_until_some(what, async || {
        let index = calendar.load().await.expect("load calendar");
        check(&index).then_some(index)
    })
    .await
}

/// Loads the scheduling index of `room` until `check` accepts it.
async fn load_scheduling_until(
    user: &TestUser,
    room: &Room,
    what: &str,
    mut check: impl FnMut(&SchedulingIndex) -> bool,
) -> SchedulingIndex {
    user.sync_until_some(what, async || {
        let index = SchedulingIndex::load(room, None)
            .await
            .expect("load scheduling index");
        check(&index).then_some(index)
    })
    .await
}

/// Fetches the event as the server stores it (without decryption).
async fn raw_server_event(
    client: &Client,
    room_id: &RoomId,
    event_id: &OwnedEventId,
) -> Raw<AnyTimelineEvent> {
    client
        .send(get_room_event::v3::Request::new(
            room_id.to_owned(),
            event_id.clone(),
        ))
        .await
        .unwrap_or_else(|e| panic!("fetching {event_id}: {e}"))
        .event
}

fn event_type(raw: &Raw<AnyTimelineEvent>) -> String {
    raw.get_field::<String>("type")
        .expect("valid event")
        .expect("event has a type")
}

fn key(user: &TestUser, uid: &str) -> EventKey {
    EventKey::new(user.user_id.clone(), uid)
}

// ---------------------------------------------------------------------------------------------
// 1. Calendar creation and discovery
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_and_discover_calendar() {
    run("create_and_discover_calendar", |ctx| async move {
        let alice = ctx.login("alice").await;

        let name = ctx.unique("discover");
        let mut opts = CreateCalendarOptions::new(&name);
        opts.info = CalendarInfo::new(Some("#3366cc".into()), Some(Berlin)).unwrap();
        let created = Calendar::create(&alice.client, opts).await.unwrap();
        let room_id = created.room_id().to_owned();
        ctx.track_room(&room_id);

        alice
            .sync_until("the calendar to be listed", |c| {
                list_calendars(c).iter().any(|cal| cal.room_id() == room_id)
            })
            .await;
        let calendar = Calendar::try_from_room(alice.room(&room_id)).expect("a calendar room");
        assert_eq!(calendar.room().name().as_deref(), Some(name.as_str()));

        // calendar_info round-trips through the server.
        let info = calendar.info().await.unwrap().expect("calendar_info state");
        assert_eq!(info.name(), Some(name.as_str()));
        assert_eq!(info.info.color.as_deref(), Some("#3366cc"));
        assert_eq!(info.info.timezone, Some(Berlin));
        assert_eq!(calendar.default_timezone().await.unwrap(), Berlin);

        let updated = CalendarInfoEventContent::new(
            &name,
            CalendarInfo::new(Some("#cc3366".into()), Some(chrono_tz::America::New_York)).unwrap(),
        )
        .unwrap();
        calendar.set_info(updated.clone()).await.unwrap();
        alice
            .sync_until_some("the updated calendar_info", async || {
                let info = calendar.info().await.unwrap()?;
                (info.info.color.as_deref() == Some("#cc3366")).then_some(())
            })
            .await;
        let info = calendar.info().await.unwrap().unwrap();
        assert_eq!(info.info, updated.info);
        assert_eq!(
            calendar.default_timezone().await.unwrap(),
            chrono_tz::America::New_York
        );

        // The room type as the server stores it in the create event.
        let state = alice
            .client
            .send(get_state_events::v3::Request::new(room_id.clone()))
            .await
            .unwrap()
            .room_state;
        let create = state
            .iter()
            .find(|ev| {
                ev.get_field::<String>("type").ok().flatten().as_deref() == Some("m.room.create")
            })
            .expect("m.room.create in the room state");
        let content: Value = create.get_field("content").unwrap().unwrap();
        ctx.log(&format!("create event content: {content}"));
        assert_eq!(content["type"], json!(ids::ROOM_TYPE_CALENDAR_UNSTABLE));

        // A normal room is not a calendar.
        let mut request = create_room::v3::Request::new();
        request.name = Some(ctx.unique("plain"));
        let plain = alice.client.create_room(request).await.unwrap();
        let plain_id = plain.room_id().to_owned();
        ctx.track_room(&plain_id);
        alice
            .sync_until("the plain room to be joined", |c| {
                c.get_room(&plain_id)
                    .is_some_and(|r| r.state() == RoomState::Joined && r.name().is_some())
            })
            .await;
        match Calendar::try_from_room(alice.room(&plain_id)) {
            Err(Error::NotACalendarRoom(id)) => assert_eq!(id, plain_id),
            other => panic!("expected NotACalendarRoom, got {other:?}"),
        }
        let listed: Vec<OwnedRoomId> = list_calendars(&alice.client)
            .iter()
            .map(|c| c.room_id().to_owned())
            .collect();
        assert!(listed.contains(&room_id));
        assert!(!listed.contains(&plain_id));
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 2. A calendar shared by two users
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_calendar_two_users() {
    run("shared_calendar_two_users", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));

        let mut opts = CreateCalendarOptions::new(ctx.unique("shared"));
        opts.info = CalendarInfo::new(None, Some(Berlin)).unwrap();
        opts.invite = vec![bob.user_id.clone()];
        let calendar = Calendar::create(&alice.client, opts).await.unwrap();
        let room_id = calendar.room_id().to_owned();
        ctx.track_room(&room_id);

        // Bob sees the invite, joins and recognises the room as a calendar.
        bob.sync_until("the invite", |c| {
            c.get_room(&room_id)
                .is_some_and(|r| r.state() == RoomState::Invited)
        })
        .await;
        ctx.log(&format!(
            "invited room recognised as a calendar from stripped state: {}",
            Calendar::try_from_room(bob.room(&room_id)).is_ok()
        ));
        let bob_calendar = bob.join_calendar(&room_id).await;
        assert!(
            list_calendars(&bob.client)
                .iter()
                .any(|c| c.room_id() == room_id)
        );
        assert_eq!(bob_calendar.default_timezone().await.unwrap(), Berlin);

        // A one-off event in the DST transition week and a weekly series.
        let mut one_off = timed_event(
            "Dentist",
            &alice.user_id,
            "2026-10-21T14:00:00",
            "2026-10-21T15:00:00",
        );
        one_off.description = Some("Bring the insurance card".into());
        one_off.location = Some(Location::physical("Praxis Dr. Zahn", Some((53.55, 10.0))));
        let weekly = weekly_event(
            "Weekly planning",
            &alice.user_id,
            "2026-10-12T10:00:00",
            "2026-10-12T11:00:00",
            Weekday::Monday,
        );
        let one_off_id = calendar.create_event(one_off.clone()).await.unwrap();
        let weekly_id = calendar.create_event(weekly.clone()).await.unwrap();

        let index = load_until(&bob, &bob_calendar, "both events", |i| i.len() == 2).await;
        let entry = index
            .get(&key(&alice, &one_off.uid))
            .expect("one-off event");
        assert_eq!(entry.event_id, one_off_id);
        assert_eq!(entry.sender, alice.user_id);
        assert_eq!(entry.organizer(), alice.user_id);
        assert_eq!(entry.content, one_off);
        assert_eq!(entry.last_edit_event_id, None);
        let entry = index.get(&key(&alice, &weekly.uid)).expect("weekly event");
        assert_eq!(entry.event_id, weekly_id);
        assert_eq!(entry.content, weekly);
        assert!(entry.overrides.is_empty());

        // Five weeks across the end of DST on 2026-10-25: the series stays at 10:00 local
        // time, so it moves from 08:00Z to 09:00Z.
        let (start, end) = (utc("2026-10-12T00:00:00Z"), utc("2026-11-16T00:00:00Z"));
        let occurrences = index.occurrences(start, end, Berlin).unwrap();
        let w = weekly.uid.clone();
        let o = one_off.uid.clone();
        let expected = vec![
            (
                w.clone(),
                local("2026-10-12T10:00:00"),
                utc("2026-10-12T08:00:00Z"),
            ),
            (
                w.clone(),
                local("2026-10-19T10:00:00"),
                utc("2026-10-19T08:00:00Z"),
            ),
            (
                o.clone(),
                local("2026-10-21T14:00:00"),
                utc("2026-10-21T12:00:00Z"),
            ),
            (
                w.clone(),
                local("2026-10-26T10:00:00"),
                utc("2026-10-26T09:00:00Z"),
            ),
            (
                w.clone(),
                local("2026-11-02T10:00:00"),
                utc("2026-11-02T09:00:00Z"),
            ),
            (
                w.clone(),
                local("2026-11-09T10:00:00"),
                utc("2026-11-09T09:00:00Z"),
            ),
        ];
        assert_eq!(summarize(&occurrences, Berlin), expected);
        for occ in occurrences.iter().filter(|o| o.occurrence.uid == w) {
            assert_eq!(occ.event_id, weekly_id);
            assert_eq!(occ.override_event_id, None);
            assert_eq!(
                occ.end_utc(Berlin) - occ.start_utc(Berlin),
                chrono::TimeDelta::hours(1)
            );
            assert_eq!(
                localtime(&occ.occurrence.end).time(),
                NaiveTime::from_hms_opt(11, 0, 0).unwrap()
            );
        }

        // Alice's view and the convenience method agree with Bob's index.
        let alice_occurrences = calendar.occurrences(start, end).await.unwrap();
        assert_eq!(summarize(&alice_occurrences, Berlin), expected);
        let bob_occurrences = bob_calendar.occurrences(start, end).await.unwrap();
        assert_eq!(bob_occurrences, occurrences);
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 3. Edits, cancellation, and edits by other users
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edit_and_cancel() {
    run("edit_and_cancel", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));
        let (calendar, members) = shared_calendar(&ctx, &alice, &[&bob], false).await;
        let bob_calendar = &members[0];

        let original = timed_event(
            "Planning",
            &alice.user_id,
            "2026-11-10T10:00:00",
            "2026-11-10T11:00:00",
        );
        let k = key(&alice, &original.uid);
        let original_id = calendar.create_event(original.clone()).await.unwrap();
        load_until(&bob, bob_calendar, "the original event", |i| {
            i.get(&k).is_some()
        })
        .await;

        // Alice edits title and time.
        let mut edited = original.clone();
        edited.title = "Planning (moved)".into();
        edited.start = berlin("2026-11-10T13:00:00");
        edited.end = berlin("2026-11-10T14:30:00");
        let edit_id = calendar
            .update_event(&original_id, edited.clone())
            .await
            .unwrap();
        let index = load_until(&bob, bob_calendar, "the edit", |i| {
            i.get(&k)
                .is_some_and(|e| e.content.title == "Planning (moved)")
        })
        .await;
        let entry = index.get(&k).unwrap();
        assert_eq!(entry.event_id, original_id);
        assert_eq!(entry.last_edit_event_id.as_ref(), Some(&edit_id));
        assert_eq!(entry.content, edited);
        assert_eq!(index.by_event_id(&original_id).map(|e| &e.key), Some(&k));
        let (start, end) = (utc("2026-11-09T00:00:00Z"), utc("2026-11-16T00:00:00Z"));
        let occurrences = index.occurrences(start, end, Berlin).unwrap();
        assert_eq!(
            summarize(&occurrences, Berlin),
            vec![(
                original.uid.clone(),
                local("2026-11-10T13:00:00"),
                utc("2026-11-10T12:00:00Z")
            )]
        );

        // Bob may not edit Alice's event through the SDK.
        let mut hijacked = edited.clone();
        hijacked.title = "Hijacked by Bob".into();
        match bob_calendar
            .update_event(&original_id, hijacked.clone())
            .await
        {
            Err(Error::PermissionDenied(msg)) => ctx.log(&format!("update_event refused: {msg}")),
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
        match bob_calendar
            .cancel_event(&original_id, edited.clone())
            .await
        {
            Err(Error::PermissionDenied(_)) => {}
            other => panic!("expected PermissionDenied, got {other:?}"),
        }

        // ... and a raw m.replace edit sent by Bob is ignored by everyone.
        let content = serde_json::to_value(
            CalendarEventEventContent::new(hijacked).into_replacement(original_id.clone()),
        )
        .unwrap();
        assert_eq!(content["m.relates_to"]["rel_type"], json!("m.replace"));
        let bob_room = bob_calendar.room().clone();
        let hijack_id = bob_room
            .send_raw(ids::CALENDAR_EVENT_UNSTABLE, content)
            .await
            .unwrap()
            .response
            .event_id;
        // The server accepted it and Alice can see it.
        let raw = raw_server_event(&alice.client, &room_id_of(&calendar), &hijack_id).await;
        assert_eq!(event_type(&raw), ids::CALENDAR_EVENT_UNSTABLE);
        for (who, cal) in [("alice", &calendar), ("bob", bob_calendar)] {
            let index = cal.load().await.unwrap();
            let entry = index
                .get(&k)
                .unwrap_or_else(|| panic!("{who}: event missing"));
            assert_eq!(entry.content, edited, "{who} applied Bob's edit");
            assert_eq!(entry.last_edit_event_id.as_ref(), Some(&edit_id), "{who}");
        }

        // Alice cancels; the event stays in the index but has no occurrences.
        let cancel_id = calendar
            .cancel_event(&original_id, edited.clone())
            .await
            .unwrap();
        let index = load_until(&bob, bob_calendar, "the cancellation", |i| {
            i.get(&k).is_some_and(|e| e.content.is_cancelled())
        })
        .await;
        let entry = index.get(&k).unwrap();
        assert_eq!(entry.last_edit_event_id.as_ref(), Some(&cancel_id));
        assert_eq!(entry.content.title, "Planning (moved)");
        assert!(index.occurrences(start, end, Berlin).unwrap().is_empty());
        assert!(
            bob_calendar
                .occurrences(start, end)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(index.next_occurrence(start, Berlin).unwrap(), None);
    })
    .await;
}

fn room_id_of(calendar: &Calendar) -> OwnedRoomId {
    calendar.room_id().to_owned()
}

// ---------------------------------------------------------------------------------------------
// 4. Instance overrides
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overrides() {
    run("overrides", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));
        let (calendar, members) = shared_calendar(&ctx, &alice, &[&bob], false).await;
        let bob_calendar = &members[0];

        let series = weekly_event(
            "Standup",
            &alice.user_id,
            "2026-10-12T10:00:00",
            "2026-10-12T10:30:00",
            Weekday::Monday,
        );
        let k = key(&alice, &series.uid);
        let series_id = calendar.create_event(series.clone()).await.unwrap();
        let (start, end) = (utc("2026-10-12T00:00:00Z"), utc("2026-11-16T00:00:00Z"));

        let index = load_until(&alice, &calendar, "the series", |i| i.get(&k).is_some()).await;
        let occurrences = index.occurrences(start, end, Berlin).unwrap();
        assert_eq!(occurrences.len(), 5);

        // Move the second instance (19 Oct) and cancel the third (26 Oct, after DST ends).
        let moved_id = calendar
            .create_override_for(&occurrences[1], |ev| {
                ev.title = "Standup (moved)".into();
                ev.start = berlin("2026-10-19T11:30:00");
                ev.end = berlin("2026-10-19T12:00:00");
            })
            .await
            .unwrap();
        let cancelled_id = calendar.cancel_occurrence(&occurrences[2]).await.unwrap();

        let index = load_until(&bob, bob_calendar, "both overrides", |i| {
            i.get(&k).is_some_and(|e| e.overrides.len() == 2)
        })
        .await;
        let entry = index.get(&k).unwrap();
        assert_eq!(entry.event_id, series_id);
        assert_eq!(entry.overrides[0].event_id, moved_id);
        assert_eq!(
            entry.overrides[0].recurrence_id,
            utc("2026-10-19T08:00:00Z")
        );
        assert_eq!(entry.overrides[1].event_id, cancelled_id);
        assert_eq!(
            entry.overrides[1].recurrence_id,
            utc("2026-10-26T09:00:00Z")
        );
        assert!(entry.overrides[1].content.is_cancelled());

        let occurrences = index.occurrences(start, end, Berlin).unwrap();
        let uid = series.uid.clone();
        assert_eq!(
            summarize(&occurrences, Berlin),
            vec![
                (
                    uid.clone(),
                    local("2026-10-12T10:00:00"),
                    utc("2026-10-12T08:00:00Z")
                ),
                (
                    uid.clone(),
                    local("2026-10-19T11:30:00"),
                    utc("2026-10-19T09:30:00Z")
                ),
                (
                    uid.clone(),
                    local("2026-11-02T10:00:00"),
                    utc("2026-11-02T09:00:00Z")
                ),
                (
                    uid.clone(),
                    local("2026-11-09T10:00:00"),
                    utc("2026-11-09T09:00:00Z")
                ),
            ]
        );
        let moved = &occurrences[1];
        assert!(moved.occurrence.is_override);
        assert_eq!(moved.occurrence.event.title, "Standup (moved)");
        assert_eq!(moved.override_event_id.as_ref(), Some(&moved_id));
        assert_eq!(
            moved.occurrence.recurrence_id,
            Some(utc("2026-10-19T08:00:00Z"))
        );

        // Alice moves the overridden instance again: this edits the existing override.
        let index = load_until(&alice, &calendar, "alice's overrides", |i| {
            i.get(&k).is_some_and(|e| e.overrides.len() == 2)
        })
        .await;
        let occurrence = index
            .occurrences(start, end, Berlin)
            .unwrap()
            .into_iter()
            .find(|o| o.occurrence.recurrence_id == Some(utc("2026-10-19T08:00:00Z")))
            .expect("the moved occurrence");
        assert_eq!(occurrence.override_event_id.as_ref(), Some(&moved_id));
        let edit_id = calendar
            .create_override_for(&occurrence, |ev| {
                ev.title = "Standup (moved twice)".into();
                ev.start = berlin("2026-10-19T15:00:00");
                ev.end = berlin("2026-10-19T15:30:00");
            })
            .await
            .unwrap();

        let index = load_until(&bob, bob_calendar, "the edited override", |i| {
            i.get(&k).is_some_and(|e| {
                e.overrides
                    .iter()
                    .any(|o| o.last_edit_event_id.as_ref() == Some(&edit_id))
            })
        })
        .await;
        let entry = index.get(&k).unwrap();
        assert_eq!(entry.overrides.len(), 2);
        assert_eq!(entry.overrides[0].event_id, moved_id);
        assert_eq!(entry.overrides[0].content.title, "Standup (moved twice)");
        let occurrences = index.occurrences(start, end, Berlin).unwrap();
        assert_eq!(occurrences.len(), 4);
        assert_eq!(
            localtime(&occurrences[1].occurrence.start),
            local("2026-10-19T15:00:00")
        );
        assert_eq!(
            occurrences[1].start_utc(Berlin),
            utc("2026-10-19T13:00:00Z")
        );
        assert_eq!(occurrences[1].override_event_id.as_ref(), Some(&moved_id));
        assert_eq!(
            occurrences[1].occurrence.event.title,
            "Standup (moved twice)"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 5. Redaction
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redaction() {
    run("redaction", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));
        let (calendar, members) = shared_calendar(&ctx, &alice, &[&bob], false).await;
        let bob_calendar = &members[0];

        let doomed = timed_event(
            "Doomed",
            &alice.user_id,
            "2026-11-11T09:00:00",
            "2026-11-11T10:00:00",
        );
        let kept = timed_event(
            "Kept",
            &alice.user_id,
            "2026-11-12T09:00:00",
            "2026-11-12T10:00:00",
        );
        let doomed_id = calendar.create_event(doomed.clone()).await.unwrap();
        let kept_id = calendar.create_event(kept.clone()).await.unwrap();
        // An edit of `kept` that is redacted again: the original content comes back.
        let mut kept_edit = kept.clone();
        kept_edit.title = "Kept (edited)".into();
        let kept_edit_id = calendar.update_event(&kept_id, kept_edit).await.unwrap();
        load_until(&bob, bob_calendar, "both events", |i| {
            i.len() == 2
                && i.get(&key(&alice, &kept.uid))
                    .is_some_and(|e| e.last_edit_event_id.as_ref() == Some(&kept_edit_id))
        })
        .await;

        calendar
            .delete_event(&doomed_id, Some("e2e redaction".into()))
            .await
            .unwrap();
        calendar.delete_event(&kept_edit_id, None).await.unwrap();

        // How the server represents the redaction (room version 11+: `content.redacts`).
        let redacted = raw_server_event(&alice.client, &room_id_of(&calendar), &doomed_id).await;
        let unsigned: Value = redacted.get_field("unsigned").unwrap().unwrap_or_default();
        ctx.log(&format!(
            "redacted event: content={} redacted_because={}",
            redacted
                .get_field::<Value>("content")
                .unwrap()
                .unwrap_or_default(),
            unsigned
                .get("redacted_because")
                .cloned()
                .unwrap_or_default()
        ));
        assert!(unsigned.get("redacted_because").is_some());

        // A freshly loaded index no longer has the event, and the edit is gone.
        let index = load_until(&bob, bob_calendar, "the redactions", |i| {
            i.len() == 1
                && i.get(&key(&alice, &kept.uid))
                    .is_some_and(|e| e.last_edit_event_id.is_none())
        })
        .await;
        assert!(index.get(&key(&alice, &doomed.uid)).is_none());
        assert!(index.by_event_id(&doomed_id).is_none());
        assert_eq!(index.get(&key(&alice, &kept.uid)).unwrap().content, kept);

        // Alice's own freshly loaded index agrees. (Redactions arriving through sync are
        // covered by `live_calendar`.)
        let alice_index = calendar.load().await.unwrap();
        assert_eq!(alice_index.len(), 1);

        // Editing a redacted event is refused.
        match calendar.update_event(&doomed_id, doomed.clone()).await {
            Err(Error::Other(msg)) => ctx.log(&format!("edit of redacted event refused: {msg}")),
            other => panic!("expected an error, got {other:?}"),
        }
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 6. Live calendar
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_calendar() {
    run("live_calendar", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));
        let (calendar, members) = shared_calendar(&ctx, &alice, &[&bob], false).await;
        let bob_calendar = members[0].clone();

        // One event exists before Bob opens the live index.
        let early = timed_event(
            "Early",
            &alice.user_id,
            "2026-11-02T09:00:00",
            "2026-11-02T10:00:00",
        );
        calendar.create_event(early.clone()).await.unwrap();
        load_until(&bob, &bob_calendar, "the early event", |i| i.len() == 1).await;

        alice.start_sync();
        bob.start_sync();
        let live = bob_calendar.live(LoadOptions::default()).await.unwrap();
        let mut changes = live.subscribe();
        assert_eq!(live.read().await.len(), 1);
        assert!(live.read().await.get(&key(&alice, &early.uid)).is_some());

        // Alice creates an event; Bob's live index picks it up from sync.
        let mut event = timed_event(
            "Live",
            &alice.user_id,
            "2026-11-03T09:00:00",
            "2026-11-03T10:00:00",
        );
        event.transparency = Transparency::Opaque;
        let k = key(&alice, &event.uid);
        let event_id = calendar.create_event(event.clone()).await.unwrap();
        wait_for_change(&mut changes, &k).await;
        {
            let index = live.read().await;
            let entry = index.get(&k).expect("the new event");
            assert_eq!(entry.event_id, event_id);
            assert_eq!(entry.content, event);
            assert_eq!(index.len(), 2);
        }

        // Alice edits it; the live index follows without reloading.
        let mut edited = event.clone();
        edited.title = "Live (edited)".into();
        edited.status = Status::Tentative;
        let edit_id = calendar
            .update_event(&event_id, edited.clone())
            .await
            .unwrap();
        wait_for_change(&mut changes, &k).await;
        {
            let index = live.read().await;
            let entry = index.get(&k).unwrap();
            assert_eq!(entry.content, edited);
            assert_eq!(entry.last_edit_event_id.as_ref(), Some(&edit_id));
            assert_eq!(index.len(), 2);
        }

        // A redaction also comes through sync.
        calendar.delete_event(&event_id, None).await.unwrap();
        wait_for_change(&mut changes, &k).await;
        assert!(live.read().await.get(&k).is_none());
        assert_eq!(live.read().await.len(), 1);
    })
    .await;
}

/// Waits for a change notification for `key`.
async fn wait_for_change(
    changes: &mut broadcast::Receiver<matrix_calendar_sdk::calendar::IndexChange>,
    key: &EventKey,
) {
    let wait = async {
        loop {
            match changes.recv().await {
                Ok(change) if change.key == *key => return,
                Ok(_) => {}
                Err(e) => panic!("change channel failed: {e}"),
            }
        }
    };
    tokio::time::timeout(TIMEOUT, wait)
        .await
        .unwrap_or_else(|_| panic!("timed out after {TIMEOUT:?} waiting for a change of {key:?}"));
}

// ---------------------------------------------------------------------------------------------
// 7. Encrypted calendar
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encrypted_calendar() {
    run("encrypted_calendar", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));
        // Bob joins before anything is sent, and Alice waits until she sees him joined, so
        // that the room key is shared with his (already uploaded) device.
        let (calendar, members) = shared_calendar(&ctx, &alice, &[&bob], true).await;
        let bob_calendar = &members[0];
        let room_id = room_id_of(&calendar);
        assert!(
            calendar
                .room()
                .latest_encryption_state()
                .await
                .unwrap()
                .is_encrypted()
        );

        let weekly = weekly_event(
            "Secret planning",
            &alice.user_id,
            "2026-10-12T10:00:00",
            "2026-10-12T11:00:00",
            Weekday::Monday,
        );
        let k = key(&alice, &weekly.uid);
        let event_id = calendar.create_event(weekly.clone()).await.unwrap();

        // On the wire (as the server stores it), the event is encrypted.
        let raw = raw_server_event(&alice.client, &room_id, &event_id).await;
        assert_eq!(event_type(&raw), "m.room.encrypted");
        let wire = raw.json().get();
        assert!(
            !wire.contains("Secret planning"),
            "plaintext leaked: {wire}"
        );
        assert!(
            !wire.contains(ids::CALENDAR_EVENT_UNSTABLE),
            "event type leaked: {wire}"
        );
        let content: Value = raw.get_field("content").unwrap().unwrap();
        assert_eq!(content["algorithm"], json!("m.megolm.v1.aes-sha2"));

        // Alice's SDK decrypts it.
        let event = calendar.room().event(&event_id, None).await.unwrap();
        assert!(event.encryption_info().is_some(), "alice: not decrypted");
        assert_eq!(
            event.raw().get_field::<String>("type").unwrap().as_deref(),
            Some(ids::CALENDAR_EVENT_UNSTABLE)
        );

        // Bob decrypts and indexes it once the room key has arrived by to-device message.
        let index = load_until(&bob, bob_calendar, "the decrypted event", |i| {
            i.get(&k).is_some()
        })
        .await;
        let entry = index.get(&k).unwrap();
        assert_eq!(entry.content, weekly);
        assert_eq!(entry.event_id, event_id);
        let bob_event = bob_calendar.room().event(&event_id, None).await.unwrap();
        assert!(bob_event.encryption_info().is_some(), "bob: not decrypted");

        let (start, end) = (utc("2026-10-12T00:00:00Z"), utc("2026-11-16T00:00:00Z"));
        let occurrences = index.occurrences(start, end, Berlin).unwrap();
        assert_eq!(
            occurrences
                .iter()
                .map(|o| o.start_utc(Berlin))
                .collect::<Vec<_>>(),
            vec![
                utc("2026-10-12T08:00:00Z"),
                utc("2026-10-19T08:00:00Z"),
                utc("2026-10-26T09:00:00Z"),
                utc("2026-11-02T09:00:00Z"),
                utc("2026-11-09T09:00:00Z"),
            ]
        );

        // An encrypted edit: the relation stays readable for the index after decryption.
        let mut edited = weekly.clone();
        edited.title = "Secret planning (edited)".into();
        let edit_id = calendar
            .update_event(&event_id, edited.clone())
            .await
            .unwrap();
        let raw_edit = raw_server_event(&alice.client, &room_id, &edit_id).await;
        assert_eq!(event_type(&raw_edit), "m.room.encrypted");
        let index = load_until(&bob, bob_calendar, "the decrypted edit", |i| {
            i.get(&k)
                .is_some_and(|e| e.last_edit_event_id.as_ref() == Some(&edit_id))
        })
        .await;
        assert_eq!(index.get(&k).unwrap().content, edited);

        // The live index applies encrypted events received through sync (decrypted by
        // matrix-sdk before the event handler sees them).
        alice.start_sync();
        bob.start_sync();
        let live = bob_calendar.live(LoadOptions::default()).await.unwrap();
        let mut changes = live.subscribe();
        assert_eq!(live.read().await.get(&k).unwrap().content, edited);
        let later = timed_event(
            "Secret lunch",
            &alice.user_id,
            "2026-10-14T12:00:00",
            "2026-10-14T13:00:00",
        );
        let later_key = key(&alice, &later.uid);
        let later_id = calendar.create_event(later.clone()).await.unwrap();
        wait_for_change(&mut changes, &later_key).await;
        let index = live.read().await;
        assert_eq!(index.get(&later_key).unwrap().event_id, later_id);
        assert_eq!(index.get(&later_key).unwrap().content, later);
        assert_eq!(index.len(), 2);
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 8. Invites and RSVPs in a DM
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invite_and_rsvp_in_dm() {
    run("invite_and_rsvp_in_dm", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));

        // Alice's own calendar with the event she invites Bob to.
        let mut opts = CreateCalendarOptions::new(ctx.unique("private"));
        opts.info = CalendarInfo::new(None, Some(Berlin)).unwrap();
        let calendar = Calendar::create(&alice.client, opts).await.unwrap();
        ctx.track_room(calendar.room_id());
        let event = timed_event(
            "Lunch",
            &alice.user_id,
            "2026-11-04T12:00:00",
            "2026-11-04T13:00:00",
        );
        calendar.create_event(event.clone()).await.unwrap();
        let k = key(&alice, &event.uid);

        // A new (encrypted) DM.
        let dm = alice.client.create_dm(&bob.user_id).await.unwrap();
        let dm_id = dm.room_id().to_owned();
        ctx.track_room(&dm_id);
        let bob_dm = bob.join(&dm_id).await;
        alice
            .wait_for_members(&dm_id, std::slice::from_ref(&bob.user_id))
            .await;
        assert!(dm.latest_encryption_state().await.unwrap().is_encrypted());

        // REQUEST, sequence 0.
        let invite = CalendarInvite::request(&event, 0);
        let invite_id = scheduling::send_invite(&dm, invite.clone()).await.unwrap();
        let index = load_scheduling_until(&bob, &bob_dm, "the invite", |i| {
            i.latest_invite(&k).is_some()
        })
        .await;
        let latest = index.latest_invite(&k).unwrap();
        assert_eq!(latest.event_id, invite_id);
        assert_eq!(latest.sender, alice.user_id);
        assert_eq!(latest.organizer(), alice.user_id);
        assert_eq!(latest.invite, invite);
        assert!(!index.is_cancelled(&k));

        // Bob accepts with a comment.
        let rsvp_id = scheduling::respond(
            &bob_dm,
            &invite_id,
            PartStat::Accepted,
            Some("See you there".into()),
        )
        .await
        .unwrap();
        let index = load_scheduling_until(&alice, &dm, "Bob's acceptance", |i| {
            !i.attendees(&k).is_empty()
        })
        .await;
        let attendees = index.attendees(&k);
        assert_eq!(attendees.len(), 1);
        assert_eq!(attendees[0].user, bob.user_id);
        assert_eq!(attendees[0].partstat, PartStat::Accepted);
        assert_eq!(attendees[0].comment.as_deref(), Some("See you there"));
        assert_eq!(attendees[0].sequence, 0);
        assert!(!attendees[0].stale);
        assert_eq!(index.responses(&k)[0].event_id, rsvp_id);
        assert_eq!(index.responses(&k)[0].invite_event_id, invite_id);
        assert_eq!(index.unresolved_responses(), 0);

        // A revised invite (sequence 1) makes Bob's answer stale.
        let mut revised_event = event.clone();
        revised_event.start = berlin("2026-11-04T12:30:00");
        revised_event.end = berlin("2026-11-04T13:30:00");
        let revised = CalendarInvite::request(&revised_event, 1);
        let revised_id = scheduling::send_invite(&dm, revised.clone()).await.unwrap();
        let index = load_scheduling_until(&bob, &bob_dm, "the revised invite", |i| {
            i.latest_invite(&k).is_some_and(|r| r.invite.sequence == 1)
        })
        .await;
        assert_eq!(index.latest_invite(&k).unwrap().event_id, revised_id);
        assert_eq!(index.invites(&k).len(), 2);
        let attendees = index.attendees(&k);
        assert_eq!(attendees[0].partstat, PartStat::Accepted);
        assert!(attendees[0].stale);

        // Bob answers the revision tentatively.
        scheduling::respond(&bob_dm, &revised_id, PartStat::Tentative, None)
            .await
            .unwrap();
        let index = load_scheduling_until(&alice, &dm, "Bob's tentative answer", |i| {
            i.attendees(&k)
                .first()
                .is_some_and(|a| a.partstat == PartStat::Tentative)
        })
        .await;
        let attendees = index.attendees(&k);
        assert_eq!(attendees.len(), 1);
        assert_eq!(attendees[0].sequence, 1);
        assert_eq!(attendees[0].comment, None);
        assert!(!attendees[0].stale);
        assert_eq!(index.responses(&k).len(), 2);

        // Alice cancels.
        let latest = index.latest_invite(&k).unwrap().invite.clone();
        let cancel_id = scheduling::cancel_invite(&dm, &latest).await.unwrap();
        for (who, user, room) in [("alice", &alice, &dm), ("bob", &bob, &bob_dm)] {
            let index =
                load_scheduling_until(user, room, &format!("{who}: the cancellation"), |i| {
                    i.is_cancelled(&k)
                })
                .await;
            let latest = index.latest_invite(&k).unwrap();
            assert_eq!(latest.event_id, cancel_id, "{who}");
            assert_eq!(latest.invite.method, InviteMethod::Cancel, "{who}");
            assert_eq!(latest.invite.sequence, 2, "{who}");
            assert!(index.attendees(&k)[0].stale, "{who}");
        }

        // A cancellation cannot be answered.
        match scheduling::respond(&bob_dm, &cancel_id, PartStat::Accepted, None).await {
            Err(Error::Other(msg)) => ctx.log(&format!("respond to CANCEL refused: {msg}")),
            other => panic!("expected an error, got {other:?}"),
        }
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 9. Spoofed scheduling messages
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduling_spoofing() {
    run("scheduling_spoofing", |ctx| async move {
        let (alice, bob, mallory) =
            tokio::join!(ctx.login("alice"), ctx.login("bob"), ctx.login("mallory"));
        let (room, members) = shared_room(&ctx, &alice, &[&bob, &mallory]).await;
        let (bob_room, mallory_room) = (&members[0], &members[1]);

        let event = timed_event(
            "Board meeting",
            &alice.user_id,
            "2026-11-05T15:00:00",
            "2026-11-05T16:00:00",
        );
        let k = key(&alice, &event.uid);
        let invite = CalendarInvite::request(&event, 0);
        let invite_id = scheduling::send_invite(&room, invite.clone())
            .await
            .unwrap();
        load_scheduling_until(&mallory, mallory_room, "the invite", |i| {
            i.latest_invite(&k).is_some()
        })
        .await;

        // The SDK refuses to send a CANCEL on behalf of another organiser.
        match scheduling::cancel_invite(mallory_room, &invite).await {
            Err(Error::PermissionDenied(msg)) => ctx.log(&format!("cancel_invite refused: {msg}")),
            other => panic!("expected PermissionDenied, got {other:?}"),
        }

        // Mallory sends a CANCEL claiming Alice as organiser by hand, with a high sequence.
        let spoofed_cancel = CalendarInvite {
            uid: event.uid.clone(),
            sequence: 99,
            method: InviteMethod::Cancel,
            calendar_event: (&event).into(),
        };
        assert_eq!(
            spoofed_cancel.calendar_event.organizer.as_ref(),
            Some(&alice.user_id)
        );
        let content =
            serde_json::to_value(CalendarInviteEventContent::new(spoofed_cancel)).unwrap();
        let cancel_id = mallory_room
            .send_raw(ids::CALENDAR_INVITE_UNSTABLE, content)
            .await
            .unwrap()
            .response
            .event_id;
        // ... and a REQUEST with the same uid that names no organiser: it is filed under
        // Mallory's own key and does not touch Alice's event.
        let mut own = event.clone();
        own.organizer = None;
        own.title = "Board meeting (hijacked)".into();
        let content = serde_json::to_value(CalendarInviteEventContent::new(
            CalendarInvite::request(&own, 100),
        ))
        .unwrap();
        let hijack_id = mallory_room
            .send_raw(ids::CALENDAR_INVITE_UNSTABLE, content)
            .await
            .unwrap()
            .response
            .event_id;
        let mallory_key = key(&mallory, &event.uid);

        for (who, user, r) in [("alice", &alice, &room), ("bob", &bob, bob_room)] {
            // Mallory's REQUEST was sent last, so once it is indexed the CANCEL was seen too.
            let index = load_scheduling_until(user, r, &format!("{who}: Mallory's events"), |i| {
                i.latest_invite(&mallory_key).is_some()
            })
            .await;
            assert!(!index.is_cancelled(&k), "{who}: spoofed CANCEL accepted");
            let latest = index.latest_invite(&k).unwrap();
            assert_eq!(latest.event_id, invite_id, "{who}");
            assert_eq!(latest.invite, invite, "{who}");
            assert_eq!(index.invites(&k).len(), 1, "{who}");
            assert!(
                index.invites(&k).iter().all(|r| r.event_id != cancel_id),
                "{who}"
            );
            assert_eq!(
                index.latest_invite(&mallory_key).unwrap().event_id,
                hijack_id,
                "{who}"
            );
            assert_eq!(index.keys().count(), 2, "{who}");
        }

        // Bob can still answer Alice's invite.
        scheduling::respond(bob_room, &invite_id, PartStat::Accepted, None)
            .await
            .unwrap();
        let index = load_scheduling_until(&alice, &room, "Bob's answer", |i| {
            !i.attendees(&k).is_empty()
        })
        .await;
        assert_eq!(index.attendees(&k)[0].user, bob.user_id);
        assert!(!index.attendees(&k)[0].stale);
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 10. Availability state and busy time
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn availability_state_and_busy() {
    run("availability_state_and_busy", |ctx| async move {
        let (alice, bob, mallory) =
            tokio::join!(ctx.login("alice"), ctx.login("bob"), ctx.login("mallory"));
        let mut opts = CreateCalendarOptions::new(ctx.unique("availability"));
        opts.info = CalendarInfo::new(None, Some(Berlin)).unwrap();
        let calendar = Calendar::create(&alice.client, opts).await.unwrap();
        ctx.track_room(calendar.room_id());
        let room_id = room_id_of(&calendar);
        alice
            .sync_until("the calendar", |c| {
                c.get_room(&room_id)
                    .is_some_and(|r| r.state() == RoomState::Joined)
            })
            .await;
        let room = alice.room(&room_id);

        // Policy and working hours round-trip through room state.
        let policy = AvailabilityPolicyEventContent::new(
            0,
            vec![
                Grant::new(bob.user_id.as_str(), 2),
                Grant::new("*:other.example", 1),
            ],
        );
        let window = AvailabilityWindowEventContent::new(vec![Window {
            days: vec![
                Weekday::Monday,
                Weekday::Tuesday,
                Weekday::Wednesday,
                Weekday::Thursday,
                Weekday::Friday,
            ],
            start_time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            end_time: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
            timezone: Berlin,
        }]);
        set_policy(&room, policy.clone()).await.unwrap();
        set_window(&room, window.clone()).await.unwrap();
        alice
            .sync_until_some("the policy and window state", async || {
                let p = get_policy(&room).await.unwrap()?;
                let w = get_window(&room).await.unwrap()?;
                Some((p, w))
            })
            .await;
        assert_eq!(get_policy(&room).await.unwrap(), Some(policy.clone()));
        assert_eq!(get_window(&room).await.unwrap(), Some(window.clone()));
        let read_policy = get_policy(&room).await.unwrap().unwrap();
        assert_eq!(evaluate_tier(&read_policy, &bob.user_id), 2);
        assert_eq!(evaluate_tier(&read_policy, &mallory.user_id), 0);
        let mut wildcard = read_policy.clone();
        wildcard.grants.push(Grant::new("*:ess.localhost", 1));
        assert_eq!(evaluate_tier(&wildcard, &mallory.user_id), 1);
        assert_eq!(evaluate_tier(&wildcard, &bob.user_id), 2);

        // Working hours on Monday 2026-11-02 (CET, UTC+1).
        let read_window = get_window(&room).await.unwrap().unwrap();
        let unavailable = unavailable_from_windows(
            &read_window,
            utc("2026-11-02T00:00:00Z"),
            utc("2026-11-03T00:00:00Z"),
        );
        assert_eq!(
            unavailable
                .iter()
                .map(|i| (i.start, i.end, i.kind.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    utc("2026-11-02T00:00:00Z"),
                    utc("2026-11-02T08:00:00Z"),
                    BusyType::BusyUnavailable
                ),
                (
                    utc("2026-11-02T16:00:00Z"),
                    utc("2026-11-03T00:00:00Z"),
                    BusyType::BusyUnavailable
                ),
            ]
        );

        // Busy time for the week of 2026-11-02.
        let a = timed_event(
            "A",
            &alice.user_id,
            "2026-11-02T09:00:00",
            "2026-11-02T10:00:00",
        );
        let b = timed_event(
            "B",
            &alice.user_id,
            "2026-11-02T09:30:00",
            "2026-11-02T11:00:00",
        );
        let mut c = timed_event(
            "C",
            &alice.user_id,
            "2026-11-03T09:00:00",
            "2026-11-03T10:00:00",
        );
        c.status = Status::Tentative;
        let mut d = timed_event(
            "D",
            &alice.user_id,
            "2026-11-04T09:00:00",
            "2026-11-04T10:00:00",
        );
        d.transparency = Transparency::Transparent;
        let e = timed_event(
            "E",
            &alice.user_id,
            "2026-11-05T09:00:00",
            "2026-11-05T10:00:00",
        );
        let f = weekly_event(
            "F",
            &alice.user_id,
            "2026-10-02T14:00:00",
            "2026-10-02T15:00:00",
            Weekday::Friday,
        );
        for ev in [&a, &b, &c, &d, &f] {
            calendar.create_event(ev.clone()).await.unwrap();
        }
        let e_id = calendar.create_event(e.clone()).await.unwrap();
        calendar.cancel_event(&e_id, e.clone()).await.unwrap();

        let index = load_until(&alice, &calendar, "all six events", |i| {
            i.len() == 6
                && i.get(&key(&alice, &e.uid))
                    .is_some_and(|x| x.content.is_cancelled())
        })
        .await;
        let (start, end) = (utc("2026-11-02T00:00:00Z"), utc("2026-11-09T00:00:00Z"));
        let busy = index.busy(start, end, Berlin, false).unwrap();
        let interval = |s: &str, e: &str, kind: BusyType, title: Option<&str>| BusyInterval {
            start: utc(s),
            end: utc(e),
            kind,
            title: title.map(Into::into),
        };
        assert_eq!(
            busy,
            vec![
                interval(
                    "2026-11-02T08:00:00Z",
                    "2026-11-02T10:00:00Z",
                    BusyType::Busy,
                    None
                ),
                interval(
                    "2026-11-03T08:00:00Z",
                    "2026-11-03T09:00:00Z",
                    BusyType::BusyTentative,
                    None
                ),
                interval(
                    "2026-11-06T13:00:00Z",
                    "2026-11-06T14:00:00Z",
                    BusyType::Busy,
                    None
                ),
            ]
        );
        let titled = index.busy(start, end, Berlin, true).unwrap();
        assert_eq!(
            titled,
            vec![
                interval(
                    "2026-11-02T08:00:00Z",
                    "2026-11-02T09:00:00Z",
                    BusyType::Busy,
                    Some("A")
                ),
                interval(
                    "2026-11-02T08:30:00Z",
                    "2026-11-02T10:00:00Z",
                    BusyType::Busy,
                    Some("B")
                ),
                interval(
                    "2026-11-03T08:00:00Z",
                    "2026-11-03T09:00:00Z",
                    BusyType::BusyTentative,
                    Some("C")
                ),
                interval(
                    "2026-11-06T13:00:00Z",
                    "2026-11-06T14:00:00Z",
                    BusyType::Busy,
                    Some("F")
                ),
            ]
        );

        // The availability query endpoint is not implemented by this homeserver: a clean
        // error, not a panic. Synapse answers `404 M_UNRECOGNIZED` ("Unrecognized request").
        let result = query_availability(&bob.client, &alice.user_id, start, end).await;
        ctx.log(&format!("query_availability: {result:?}"));
        match result {
            Err(Error::Http(err)) => {
                let api = err
                    .as_client_api_error()
                    .unwrap_or_else(|| panic!("expected a client API error, got {err:?}"));
                assert_eq!(api.status_code.as_u16(), 404);
                assert_eq!(
                    api.error_kind(),
                    Some(&matrix_sdk::ruma::api::error::ErrorKind::Unrecognized)
                );
            }
            other => panic!("expected Error::Http(404 M_UNRECOGNIZED), got {other:?}"),
        }
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 11. Interop with clients that send the stable identifiers
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stable_identifiers_interop() {
    run("stable_identifiers_interop", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));
        let (calendar, members) = shared_calendar(&ctx, &alice, &[&bob], false).await;
        let bob_room = members[0].room().clone();

        // MSC4496 §2.1, verbatim except for a fresh uid and Bob as organiser.
        let single_uid = ctx.uid("single");
        let single = json!({
            "m.text": [
                {
                    "body": "Team standup – 2026-07-01 09:00–09:30 (Europe/Berlin)",
                    "mimetype": "text/plain"
                }
            ],
            "m.calendar.event": {
                "uid": single_uid,
                "title": "Team standup",
                "start": {
                    "utc": "2026-07-01T07:00:00Z",
                    "localtime": "2026-07-01T09:00:00",
                    "timezone": "Europe/Berlin"
                },
                "end": {
                    "utc": "2026-07-01T07:30:00Z",
                    "localtime": "2026-07-01T09:30:00",
                    "timezone": "Europe/Berlin"
                },
                "all_day": false,
                "status": "confirmed",
                "transparency": "opaque",
                "description": "Daily sync with engineering team.",
                "organizer": bob.user_id,
                "location": null,
                "conference": null,
                "recurrence": null
            }
        });
        let single_id = bob_room.send_raw(ids::CALENDAR_EVENT, single).await.unwrap().response.event_id;

        // A series with the §2.5 recurrence (weekly on Mondays until 2026-12-31), plus a
        // stable `m.calendar.override` of its second instance.
        let series_uid = ctx.uid("series");
        let series = json!({
            "m.text": [{ "body": "Weekly sync", "mimetype": "text/plain" }],
            "m.calendar.event": {
                "uid": series_uid,
                "title": "Weekly sync",
                "start": { "utc": "2026-07-06T07:00:00Z", "localtime": "2026-07-06T09:00:00", "timezone": "Europe/Berlin" },
                "end": { "utc": "2026-07-06T07:30:00Z", "localtime": "2026-07-06T09:30:00", "timezone": "Europe/Berlin" },
                "all_day": false,
                "status": "confirmed",
                "transparency": "opaque",
                "organizer": bob.user_id,
                "location": null,
                "conference": null,
                "recurrence": {
                    "rrule": { "freq": "WEEKLY", "byday": ["MO"], "until": "2026-12-31" },
                    "exdates": ["2026-07-20T07:00:00Z"]
                }
            }
        });
        let series_id = bob_room.send_raw(ids::CALENDAR_EVENT, series).await.unwrap().response.event_id;
        let override_content = json!({
            "m.text": [{ "body": "Weekly sync (moved)", "mimetype": "text/plain" }],
            "m.relates_to": { "rel_type": "m.calendar.override", "event_id": series_id },
            "m.calendar.event": {
                "uid": series_uid,
                "title": "Weekly sync (moved)",
                "recurrence_id": "2026-07-13T07:00:00Z",
                "start": { "utc": "2026-07-14T08:00:00Z", "localtime": "2026-07-14T10:00:00", "timezone": "Europe/Berlin" },
                "end": { "utc": "2026-07-14T08:30:00Z", "localtime": "2026-07-14T10:30:00", "timezone": "Europe/Berlin" },
                "all_day": false,
                "status": "confirmed",
                "transparency": "opaque",
                "organizer": bob.user_id,
                "location": null,
                "conference": null,
                "recurrence": null
            }
        });
        let override_id = bob_room
            .send_raw(ids::CALENDAR_EVENT, override_content)
            .await
            .unwrap()
            .response
            .event_id;

        let index = load_until(&alice, &calendar, "Bob's stable events", |i| {
            i.get(&key(&bob, &series_uid)).is_some_and(|e| e.overrides.len() == 1)
                && i.get(&key(&bob, &single_uid)).is_some()
        })
        .await;
        let entry = index.get(&key(&bob, &single_uid)).unwrap();
        assert_eq!(entry.event_id, single_id);
        assert_eq!(entry.sender, bob.user_id);
        assert_eq!(entry.content.title, "Team standup");
        assert_eq!(entry.content.description.as_deref(), Some("Daily sync with engineering team."));
        assert_eq!(entry.content.start, berlin("2026-07-01T09:00:00"));
        let entry = index.get(&key(&bob, &series_uid)).unwrap();
        assert_eq!(entry.event_id, series_id);
        assert_eq!(entry.overrides[0].event_id, override_id);

        let occurrences = index
            .occurrences(utc("2026-06-29T00:00:00Z"), utc("2026-07-28T00:00:00Z"), Berlin)
            .unwrap();
        assert_eq!(
            summarize(&occurrences, Berlin),
            vec![
                (single_uid.clone(), local("2026-07-01T09:00:00"), utc("2026-07-01T07:00:00Z")),
                (series_uid.clone(), local("2026-07-06T09:00:00"), utc("2026-07-06T07:00:00Z")),
                (series_uid.clone(), local("2026-07-14T10:00:00"), utc("2026-07-14T08:00:00Z")),
                // 2026-07-20 is an exdate.
                (series_uid.clone(), local("2026-07-27T09:00:00"), utc("2026-07-27T07:00:00Z")),
            ]
        );
        assert_eq!(occurrences[2].override_event_id.as_ref(), Some(&override_id));
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 12. Pagination of a large calendar
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pagination_large_calendar() {
    const EVENTS: usize = 250;
    const BATCH: usize = 25;
    const CHAT_PER_BATCH: usize = 4;

    run("pagination_large_calendar", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));
        let (calendar, _) = shared_calendar(&ctx, &alice, &[&bob], false).await;
        let room_id = room_id_of(&calendar);
        let prefix = ctx.uid("bulk");
        let uid_of = |i: usize| format!("{prefix}-{i:03}");

        // 250 events in batches of 25 sent concurrently, with chat messages in each batch.
        let started = Instant::now();
        for batch in 0..EVENTS / BATCH {
            let mut tasks = JoinSet::new();
            for i in batch * BATCH..(batch + 1) * BATCH {
                let calendar = calendar.clone();
                let start =
                    local("2027-01-04T08:00:00") + chrono::TimeDelta::minutes(30 * i as i64);
                let mut event = CalendarEvent::new(
                    format!("Bulk event {i:03}"),
                    EventTime::timed(start, Berlin).unwrap(),
                    EventTime::timed(start + chrono::TimeDelta::minutes(15), Berlin).unwrap(),
                    alice.user_id.clone(),
                );
                event.uid = uid_of(i);
                tasks.spawn(async move { calendar.create_event(event).await.map(|_| ()) });
            }
            for j in 0..CHAT_PER_BATCH {
                let room = calendar.room().clone();
                tasks.spawn(async move {
                    room.send_raw(
                        "m.room.message",
                        json!({ "msgtype": "m.text", "body": format!("chat {batch}/{j}") }),
                    )
                    .await
                    .map(|_| ())
                    .map_err(Into::into)
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.expect("send task").expect("send");
            }
        }
        ctx.log(&format!(
            "sent {EVENTS} events and {} chat messages in {:.1}s",
            EVENTS / BATCH * CHAT_PER_BATCH,
            started.elapsed().as_secs_f64()
        ));

        // A fresh login of Bob loads the complete index (several filtered /messages pages).
        let fresh = ctx.login("bob").await;
        fresh
            .sync_until("the calendar", |c| {
                c.get_room(&room_id)
                    .is_some_and(|r| r.state() == RoomState::Joined)
            })
            .await;
        let bob_calendar = Calendar::try_from_room(fresh.room(&room_id)).expect("a calendar");
        let started = Instant::now();
        let index = bob_calendar.load().await.unwrap();
        ctx.log(&format!(
            "full load took {:.2}s",
            started.elapsed().as_secs_f64()
        ));
        assert_eq!(index.len(), EVENTS);
        for i in 0..EVENTS {
            let entry = index
                .get(&key(&alice, &uid_of(i)))
                .unwrap_or_else(|| panic!("event {i} missing"));
            assert_eq!(entry.content.title, format!("Bulk event {i:03}"));
        }

        // Smaller pages give the same result.
        let small_pages = bob_calendar
            .load_with(LoadOptions {
                page_size: Some(40),
                ..LoadOptions::default()
            })
            .await
            .unwrap();
        assert_eq!(small_pages.len(), EVENTS);

        // `max_events` stops after that many (filtered) events, newest first: the last four
        // batches. Chat messages do not count.
        let mut newest = CalendarIndex::new();
        let fed = bob_calendar
            .load_into(
                &mut newest,
                LoadOptions {
                    max_events: Some(100),
                    ..LoadOptions::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(fed, 100);
        assert_eq!(newest.len(), 100);
        for i in EVENTS - 100..EVENTS {
            assert!(
                newest.get(&key(&alice, &uid_of(i))).is_some(),
                "event {i} missing"
            );
        }

        // A limit that is not a multiple of the page size.
        let limited = bob_calendar
            .load_with(LoadOptions {
                max_events: Some(60),
                page_size: Some(25),
                ..LoadOptions::default()
            })
            .await
            .unwrap();
        assert_eq!(limited.len(), 60);
        assert!(limited.iter().all(|e| {
            let i: usize = e.key.uid.rsplit('-').next().unwrap().parse().unwrap();
            i >= EVENTS - 3 * BATCH
        }));
        for i in EVENTS - 2 * BATCH..EVENTS {
            assert!(
                limited.get(&key(&alice, &uid_of(i))).is_some(),
                "event {i} missing"
            );
        }

        // The occurrences of the whole set are all there, too.
        let occurrences = index
            .occurrences(
                utc("2027-01-01T00:00:00Z"),
                utc("2027-02-01T00:00:00Z"),
                Berlin,
            )
            .unwrap();
        assert_eq!(occurrences.len(), EVENTS);
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 13. Permissions: read-only and read-write sharing enforced by the homeserver
// ---------------------------------------------------------------------------------------------

/// Asserts that the homeserver rejected a request with `M_FORBIDDEN`.
fn assert_forbidden<T: std::fmt::Debug>(ctx: &Ctx, what: &str, result: matrix_sdk::Result<T>) {
    match result {
        Err(error) if error.client_api_error_kind() == Some(&ErrorKind::Forbidden) => {
            ctx.log(&format!("server refused {what}: {error}"));
        }
        other => panic!("expected the server to refuse {what} with M_FORBIDDEN, got {other:?}"),
    }
}

/// Asserts a client-side [`Error::PermissionDenied`].
fn assert_denied<T: std::fmt::Debug>(ctx: &Ctx, what: &str, result: Result<T, Error>) {
    match result {
        Err(Error::PermissionDenied(msg)) => ctx.log(&format!("{what} refused: {msg}")),
        other => panic!("expected PermissionDenied for {what}, got {other:?}"),
    }
}

/// Syncs `user` until their own role in `calendar` is `role`.
async fn wait_for_role(user: &TestUser, calendar: &Calendar, role: CalendarRole) {
    user.sync_until_some(&format!("the {role} role"), async || {
        (calendar.my_role().await.unwrap() == role).then_some(())
    })
    .await;
}

/// A raw calendar event content (sent with `send_raw`, bypassing the SDK's checks).
fn raw_calendar_event(organizer: &UserId) -> Value {
    serde_json::to_value(CalendarEventEventContent::new(timed_event(
        "Sneaky",
        organizer,
        "2026-12-01T10:00:00",
        "2026-12-01T11:00:00",
    )))
    .unwrap()
}

/// `(user, membership, role)` of every member, as `user` sees them.
async fn roles_seen_by(
    calendar: &Calendar,
) -> Vec<(OwnedUserId, CalendarMembership, CalendarRole)> {
    calendar
        .members_with_roles()
        .await
        .unwrap()
        .into_iter()
        .map(|m| (m.user_id, m.membership, m.role))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_sharing() {
    run("read_only_sharing", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));

        let mut opts = CreateCalendarOptions::new(ctx.unique("read-only"));
        opts.info = CalendarInfo::new(None, Some(Berlin)).unwrap();
        let calendar = Calendar::create(&alice.client, opts).await.unwrap();
        let room_id = room_id_of(&calendar);
        ctx.track_room(&room_id);
        let event = timed_event(
            "Board meeting",
            &alice.user_id,
            "2026-12-01T10:00:00",
            "2026-12-01T11:00:00",
        );
        let event_id = calendar.create_event(event.clone()).await.unwrap();

        // The one-liner: share read-only.
        calendar.invite_reader(&bob.user_id).await.unwrap();
        let bob_calendar = bob.join_calendar(&room_id).await;
        alice
            .wait_for_members(&room_id, std::slice::from_ref(&bob.user_id))
            .await;

        // Bob can read.
        let index = load_until(&bob, &bob_calendar, "the event", |i| i.len() == 1).await;
        let entry = index.get(&key(&alice, &event.uid)).expect("the event");
        assert_eq!(entry.event_id, event_id);
        assert_eq!(entry.content, event);
        assert_eq!(bob_calendar.my_role().await.unwrap(), CalendarRole::Reader);
        assert!(!bob_calendar.can_write().await.unwrap());
        assert_eq!(
            calendar.role_of(&bob.user_id).await.unwrap(),
            CalendarRole::Reader
        );
        let summary = bob_calendar.permissions().await.unwrap();
        ctx.log(&format!("permissions seen by bob: {summary:?}"));
        assert!(summary.is_standard);
        assert_eq!(summary.default_role, CalendarRole::Reader);

        // The SDK refuses to write before contacting the server ...
        let mine = timed_event(
            "Bob's event",
            &bob.user_id,
            "2026-12-02T10:00:00",
            "2026-12-02T11:00:00",
        );
        assert_denied(
            &ctx,
            "create_event as a reader",
            bob_calendar.create_event(mine).await,
        );

        // ... and the server refuses what the SDK would not send.
        let bob_room = bob_calendar.room().clone();
        assert_forbidden(
            &ctx,
            "a calendar event from a reader",
            bob_room
                .send_raw(
                    ids::CALENDAR_EVENT_UNSTABLE,
                    raw_calendar_event(&bob.user_id),
                )
                .await,
        );
        assert_forbidden(
            &ctx,
            "a stable-type calendar event from a reader",
            bob_room
                .send_raw(ids::CALENDAR_EVENT, raw_calendar_event(&bob.user_id))
                .await,
        );
        assert_forbidden(
            &ctx,
            "a chat message from a reader",
            bob_room
                .send_raw(
                    "m.room.message",
                    json!({ "msgtype": "m.text", "body": "hi" }),
                )
                .await,
        );

        // Readers may RSVP.
        let rsvp = json!({
            "m.text": [{ "body": "Bob accepted", "mimetype": "text/plain" }],
            ids::CALENDAR_RSVP_UNSTABLE: { "uid": event.uid, "partstat": "ACCEPTED" },
        });
        bob_room
            .send_raw(ids::CALENDAR_RSVP_UNSTABLE, rsvp)
            .await
            .expect("readers can RSVP");

        // Alice's calendar is unchanged.
        let alice_index = calendar.load().await.unwrap();
        assert_eq!(alice_index.len(), 1);
        assert_eq!(
            alice_index.get(&key(&alice, &event.uid)).unwrap().content,
            event
        );
        assert_eq!(
            roles_seen_by(&calendar).await,
            vec![
                (
                    alice.user_id.clone(),
                    CalendarMembership::Joined,
                    CalendarRole::Owner
                ),
                (
                    bob.user_id.clone(),
                    CalendarMembership::Joined,
                    CalendarRole::Reader
                ),
            ]
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promote_and_demote() {
    run("promote_and_demote", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));
        let calendar = Calendar::create(
            &alice.client,
            CreateCalendarOptions::new(ctx.unique("promote")),
        )
        .await
        .unwrap();
        let room_id = room_id_of(&calendar);
        ctx.track_room(&room_id);
        calendar.invite_reader(&bob.user_id).await.unwrap();
        let bob_calendar = bob.join_calendar(&room_id).await;
        assert_eq!(bob_calendar.my_role().await.unwrap(), CalendarRole::Reader);

        // Promote Bob; once his client has synced the new power levels he can write.
        calendar
            .set_role(&bob.user_id, CalendarRole::Editor)
            .await
            .unwrap();
        wait_for_role(&bob, &bob_calendar, CalendarRole::Editor).await;
        let bobs = timed_event(
            "Bob's workshop",
            &bob.user_id,
            "2026-12-03T10:00:00",
            "2026-12-03T12:00:00",
        );
        let bobs_id = bob_calendar.create_event(bobs.clone()).await.unwrap();
        let index = load_until(&alice, &calendar, "Bob's event", |i| {
            i.get(&key(&bob, &bobs.uid)).is_some()
        })
        .await;
        assert_eq!(index.get(&key(&bob, &bobs.uid)).unwrap().event_id, bobs_id);

        // Demote Bob again: his next event is refused (by the SDK and by the server).
        calendar
            .set_role(&bob.user_id, CalendarRole::Reader)
            .await
            .unwrap();
        wait_for_role(&bob, &bob_calendar, CalendarRole::Reader).await;
        let another = timed_event(
            "Bob's second workshop",
            &bob.user_id,
            "2026-12-04T10:00:00",
            "2026-12-04T12:00:00",
        );
        assert_denied(
            &ctx,
            "create_event after the demotion",
            bob_calendar.create_event(another.clone()).await,
        );
        assert_forbidden(
            &ctx,
            "a calendar event after the demotion",
            bob_calendar
                .room()
                .send_raw(
                    ids::CALENDAR_EVENT_UNSTABLE,
                    raw_calendar_event(&bob.user_id),
                )
                .await,
        );
        // Nor can he edit his earlier event any more.
        let mut moved = bobs.clone();
        moved.title = "Bob's workshop (moved)".into();
        assert_denied(
            &ctx,
            "update_event after the demotion",
            bob_calendar.update_event(&bobs_id, moved).await,
        );

        // Bob's earlier event stays in the calendar.
        for (who, cal) in [("alice", &calendar), ("bob", &bob_calendar)] {
            let index = cal.load().await.unwrap();
            let entry = index
                .get(&key(&bob, &bobs.uid))
                .unwrap_or_else(|| panic!("{who}: Bob's event is gone"));
            assert_eq!(entry.content, bobs, "{who}");
        }

        // Alice created the room (version 12), so she is an owner for good. (Refusing to
        // demote the last owner in older room versions is covered by the mocked tests.)
        assert_denied(
            &ctx,
            "the creator stepping down",
            calendar
                .set_role(&alice.user_id, CalendarRole::Editor)
                .await,
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_and_owner_roles() {
    run("editor_and_owner_roles", |ctx| async move {
        let (alice, bob, mallory) =
            tokio::join!(ctx.login("alice"), ctx.login("bob"), ctx.login("mallory"));

        let name = ctx.unique("roles");
        let mut opts = CreateCalendarOptions::new(&name);
        opts.members = vec![
            (bob.user_id.clone(), CalendarRole::Editor),
            (mallory.user_id.clone(), CalendarRole::Reader),
        ];
        let calendar = Calendar::create(&alice.client, opts).await.unwrap();
        let room_id = room_id_of(&calendar);
        ctx.track_room(&room_id);
        let bob_calendar = bob.join_calendar(&room_id).await;
        let mallory_calendar = mallory.join_calendar(&room_id).await;
        alice
            .wait_for_members(&room_id, &user_ids(&[&bob, &mallory]))
            .await;

        // Everybody sees the same roles.
        let expected = vec![
            (
                alice.user_id.clone(),
                CalendarMembership::Joined,
                CalendarRole::Owner,
            ),
            (
                bob.user_id.clone(),
                CalendarMembership::Joined,
                CalendarRole::Editor,
            ),
            (
                mallory.user_id.clone(),
                CalendarMembership::Joined,
                CalendarRole::Reader,
            ),
        ];
        for (user, cal) in [
            (&alice, &calendar),
            (&bob, &bob_calendar),
            (&mallory, &mallory_calendar),
        ] {
            user.sync_until_some("all three members with their roles", async || {
                (roles_seen_by(cal).await == expected).then_some(())
            })
            .await;
        }

        // Bob, an editor, can write events but not change the calendar info.
        bob_calendar
            .create_event(timed_event(
                "Editor's event",
                &bob.user_id,
                "2026-12-05T10:00:00",
                "2026-12-05T11:00:00",
            ))
            .await
            .unwrap();
        let new_info = CalendarInfoEventContent::new(
            &name,
            CalendarInfo::new(Some("#00aa00".into()), Some(Berlin)).unwrap(),
        )
        .unwrap();
        assert_denied(
            &ctx,
            "set_info as an editor",
            bob_calendar.set_info(new_info.clone()).await,
        );
        assert_forbidden(
            &ctx,
            "calendar_info from an editor",
            bob_calendar
                .room()
                .send_state_event_raw(
                    ids::CALENDAR_INFO_UNSTABLE,
                    "",
                    serde_json::to_value(&new_info).unwrap(),
                )
                .await,
        );
        assert_denied(
            &ctx,
            "set_role as an editor",
            bob_calendar
                .set_role(&mallory.user_id, CalendarRole::Editor)
                .await,
        );

        // Alice makes Bob an owner; then he can manage the calendar.
        calendar
            .set_role(&bob.user_id, CalendarRole::Owner)
            .await
            .unwrap();
        wait_for_role(&bob, &bob_calendar, CalendarRole::Owner).await;
        bob_calendar.set_info(new_info).await.unwrap();
        alice
            .sync_until_some("Bob's calendar_info", async || {
                let info = calendar.info().await.unwrap()?;
                (info.info.color.as_deref() == Some("#00aa00")).then_some(())
            })
            .await;
        bob_calendar
            .set_role(&mallory.user_id, CalendarRole::Editor)
            .await
            .unwrap();
        wait_for_role(&mallory, &mallory_calendar, CalendarRole::Editor).await;
        // ... but not demote the creator, who has unlimited power in room version 12.
        assert_denied(
            &ctx,
            "demoting the room creator",
            bob_calendar
                .set_role(&alice.user_id, CalendarRole::Reader)
                .await,
        );

        // Bob removes Mallory and invites her back as a reader.
        bob_calendar
            .remove_member(&mallory.user_id, Some("e2e"))
            .await
            .unwrap();
        mallory
            .sync_until("the removal", |c| {
                c.get_room(&room_id)
                    .is_some_and(|r| r.state() == RoomState::Left)
            })
            .await;
        bob_calendar.invite_reader(&mallory.user_id).await.unwrap();
        let expected = vec![
            (
                alice.user_id.clone(),
                CalendarMembership::Joined,
                CalendarRole::Owner,
            ),
            (
                bob.user_id.clone(),
                CalendarMembership::Joined,
                CalendarRole::Owner,
            ),
            (
                mallory.user_id.clone(),
                CalendarMembership::Invited,
                CalendarRole::Reader,
            ),
        ];
        alice
            .sync_until_some("Mallory invited again as a reader", async || {
                (roles_seen_by(&calendar).await == expected).then_some(())
            })
            .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_permissions_to_legacy_calendar() {
    run("apply_permissions_to_legacy_calendar", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));

        // A calendar created by "another client": Matrix's default power levels.
        let mut creation_content = create_room::v3::CreationContent::new();
        creation_content.room_type = Some(ids::ROOM_TYPE_CALENDAR_UNSTABLE.into());
        let mut request = create_room::v3::Request::new();
        request.name = Some(ctx.unique("legacy"));
        request.creation_content = Some(Raw::new(&creation_content).unwrap());
        request.invite = vec![bob.user_id.clone()];
        let room = alice.client.create_room(request).await.unwrap();
        let room_id = room.room_id().to_owned();
        ctx.track_room(&room_id);
        let bob_calendar = bob.join_calendar(&room_id).await;
        alice
            .wait_for_members(&room_id, std::slice::from_ref(&bob.user_id))
            .await;
        alice
            .sync_until("the legacy calendar", |c| {
                c.get_room(&room_id)
                    .is_some_and(|r| Calendar::try_from_room(r).is_ok())
            })
            .await;
        let calendar = Calendar::try_from_room(alice.room(&room_id)).unwrap();

        // Everyone can write: the creator is an owner, members are editors.
        assert_eq!(calendar.my_role().await.unwrap(), CalendarRole::Owner);
        assert_eq!(
            calendar.role_of(&bob.user_id).await.unwrap(),
            CalendarRole::Editor
        );
        assert_eq!(bob_calendar.my_role().await.unwrap(), CalendarRole::Editor);
        let summary = calendar.permissions().await.unwrap();
        ctx.log(&format!("legacy permissions: {summary:?}"));
        assert!(!summary.is_standard);
        assert_eq!(summary.default_role, CalendarRole::Editor);
        let early = timed_event(
            "Written before the lockdown",
            &bob.user_id,
            "2026-12-06T10:00:00",
            "2026-12-06T11:00:00",
        );
        bob_calendar.create_event(early.clone()).await.unwrap();

        // Setting a reader level alone would not make Bob a reader here.
        match calendar.set_role(&bob.user_id, CalendarRole::Reader).await {
            Err(Error::InvalidPermissions(msg)) => ctx.log(&format!("set_role refused: {msg}")),
            other => panic!("expected InvalidPermissions, got {other:?}"),
        }

        // Apply the SDK scheme with the default (reader) default role.
        calendar
            .apply_permissions(&CalendarPermissions::default())
            .await
            .unwrap();
        alice
            .sync_until_some("the new power levels", async || {
                calendar
                    .permissions()
                    .await
                    .unwrap()
                    .is_standard
                    .then_some(())
            })
            .await;
        let summary = calendar.permissions().await.unwrap();
        assert_eq!(summary.default_role, CalendarRole::Reader);
        assert_eq!(calendar.my_role().await.unwrap(), CalendarRole::Owner);

        wait_for_role(&bob, &bob_calendar, CalendarRole::Reader).await;
        assert_denied(
            &ctx,
            "create_event after apply_permissions",
            bob_calendar
                .create_event(timed_event(
                    "Too late",
                    &bob.user_id,
                    "2026-12-07T10:00:00",
                    "2026-12-07T11:00:00",
                ))
                .await,
        );
        assert_forbidden(
            &ctx,
            "a calendar event after apply_permissions",
            bob_calendar
                .room()
                .send_raw(
                    ids::CALENDAR_EVENT_UNSTABLE,
                    raw_calendar_event(&bob.user_id),
                )
                .await,
        );
        let index = calendar.load().await.unwrap();
        assert_eq!(index.get(&key(&bob, &early.uid)).unwrap().content, early);

        // Switching the default role to editor lets him write again.
        calendar
            .set_default_role(CalendarRole::Editor)
            .await
            .unwrap();
        wait_for_role(&bob, &bob_calendar, CalendarRole::Editor).await;
        assert!(calendar.permissions().await.is_ok());
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// 12. Receiving a shared calendar: invitations, accept, decline, leave
// ---------------------------------------------------------------------------------------------

/// Syncs `user` until there is a pending calendar invitation to `room_id`, and returns it.
async fn wait_for_invitation(user: &TestUser, room_id: &RoomId) -> CalendarInvitation {
    user.sync_until_some(&format!("an invitation to {room_id}"), async || {
        let mut matching: Vec<CalendarInvitation> = list_calendar_invitations(&user.client)
            .into_iter()
            .filter(|invitation| invitation.room_id() == room_id)
            .collect();
        assert!(
            matching.len() <= 1,
            "{} invitations to {room_id}",
            matching.len()
        );
        matching.pop()
    })
    .await
}

/// Whether `user` has a pending calendar invitation to `room_id`.
fn has_invitation(user: &TestUser, room_id: &RoomId) -> bool {
    list_calendar_invitations(&user.client)
        .iter()
        .any(|invitation| invitation.room_id() == room_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accept_calendar_invitation() {
    run("accept_calendar_invitation", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));

        let name = ctx.unique("shared with bob");
        let mut opts = CreateCalendarOptions::new(&name);
        opts.info = CalendarInfo::new(Some("#3366cc".into()), Some(Berlin)).unwrap();
        let calendar = Calendar::create(&alice.client, opts).await.unwrap();
        let room_id = room_id_of(&calendar);
        ctx.track_room(&room_id);
        let event = timed_event(
            "Alice's meeting",
            &alice.user_id,
            "2026-12-01T10:00:00",
            "2026-12-01T11:00:00",
        );
        calendar.create_event(event.clone()).await.unwrap();

        // Alice shares the calendar read-write.
        calendar.invite_editor(&bob.user_id).await.unwrap();

        // Bob sees exactly one pending invitation to that calendar, and nothing else about
        // this room.
        let invitation = wait_for_invitation(&bob, &room_id).await;
        assert!(
            !list_calendars(&bob.client)
                .iter()
                .any(|c| c.room_id() == room_id)
        );
        assert_eq!(invitation.name().await.as_deref(), Some(name.as_str()));
        assert_eq!(invitation.inviter().await.as_deref(), Some(&*alice.user_id));
        assert!(!invitation.is_encrypted().await);

        // Synapse's stripped invite state has `m.room.create`, `m.room.name`,
        // `m.room.join_rules` and the member events, but neither the `calendar_info` nor the
        // `m.room.power_levels` state event: the colour is unknown before joining, and the
        // role falls back to the SDK's default role (reader) even though bob was invited as
        // an editor.
        assert_eq!(invitation.color().await, None);
        assert!(invitation.info().await.unwrap().is_none());
        assert_eq!(invitation.role().await, CalendarRole::Reader);
        ctx.log(&format!(
            "invitation: name {:?}, colour {:?}, inviter {:?}, role {}",
            invitation.name().await,
            invitation.color().await,
            invitation.inviter().await,
            invitation.role().await,
        ));

        // Accepting joins the room and yields a usable calendar.
        let bob_calendar = invitation.accept().await.expect("accept the invitation");
        assert_eq!(bob_calendar.room_id(), room_id);
        assert_eq!(
            bob_calendar.my_role().await.unwrap(),
            CalendarRole::Editor,
            "the role the server enforces is known right after accepting"
        );
        assert!(bob_calendar.can_write().await.unwrap());

        // Joining marks the room as joined locally right away, so the calendar is listed
        // immediately and the invitation is gone.
        assert!(
            list_calendars(&bob.client)
                .iter()
                .any(|cal| cal.room_id() == room_id)
        );
        assert!(!has_invitation(&bob, &room_id));

        // The room's own state, and with it the colour, arrives with the next sync.
        let listed = list_calendars(&bob.client)
            .into_iter()
            .find(|cal| cal.room_id() == room_id)
            .expect("the calendar");
        let info = bob
            .sync_until_some("the calendar info", async || listed.info().await.unwrap())
            .await;
        assert_eq!(info.name(), Some(name.as_str()));
        assert_eq!(info.info.color.as_deref(), Some("#3366cc"));

        // Bob reads alice's event and adds one of his own.
        let index = load_until(&bob, &bob_calendar, "alice's event", |i| i.len() == 1).await;
        assert_eq!(index.get(&key(&alice, &event.uid)).unwrap().content, event);
        let mine = timed_event(
            "Bob's meeting",
            &bob.user_id,
            "2026-12-02T10:00:00",
            "2026-12-02T11:00:00",
        );
        bob_calendar.create_event(mine.clone()).await.unwrap();

        // Alice sees both events and bob as a joined editor.
        let alice_index = load_until(&alice, &calendar, "both events", |i| i.len() == 2).await;
        assert_eq!(
            alice_index.get(&key(&bob, &mine.uid)).unwrap().content,
            mine
        );
        alice
            .sync_until_some("bob to be a joined editor", async || {
                roles_seen_by(&calendar)
                    .await
                    .contains(&(
                        bob.user_id.clone(),
                        CalendarMembership::Joined,
                        CalendarRole::Editor,
                    ))
                    .then_some(())
            })
            .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decline_calendar_invitation() {
    run("decline_calendar_invitation", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));

        let name = ctx.unique("declined");
        let calendar = Calendar::create(&alice.client, CreateCalendarOptions::new(&name))
            .await
            .unwrap();
        let room_id = room_id_of(&calendar);
        ctx.track_room(&room_id);

        // Shared read-only this time.
        calendar.invite_reader(&bob.user_id).await.unwrap();
        alice
            .sync_until_some("bob to be invited", async || {
                roles_seen_by(&calendar)
                    .await
                    .contains(&(
                        bob.user_id.clone(),
                        CalendarMembership::Invited,
                        CalendarRole::Reader,
                    ))
                    .then_some(())
            })
            .await;

        let invitation = wait_for_invitation(&bob, &room_id).await;
        assert_eq!(invitation.name().await.as_deref(), Some(name.as_str()));
        assert_eq!(invitation.role().await, CalendarRole::Reader);
        invitation.decline().await.expect("decline the invitation");

        // The invitation is gone and bob has not joined. `Room::leave` forgets rooms that
        // were invites, so the room is dropped from the store; a later sync may bring it back
        // as a left room, which is neither invited nor joined.
        assert!(!has_invitation(&bob, &room_id));
        bob.sync_once().await;
        assert!(!has_invitation(&bob, &room_id));
        assert!(
            !list_calendars(&bob.client)
                .iter()
                .any(|c| c.room_id() == room_id)
        );
        if let Some(room) = bob.client.get_room(&room_id) {
            assert_eq!(room.state(), RoomState::Left, "bob must not be a member");
        }

        // Alice no longer lists bob at all (the declined invite is a leave).
        alice
            .sync_until_some("bob to be gone from the member list", async || {
                roles_seen_by(&calendar)
                    .await
                    .iter()
                    .all(|(user, _, _)| *user != bob.user_id)
                    .then_some(())
            })
            .await;
        assert_eq!(
            roles_seen_by(&calendar).await,
            vec![(
                alice.user_id.clone(),
                CalendarMembership::Joined,
                CalendarRole::Owner
            )]
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leave_calendar() {
    run("leave_calendar", |ctx| async move {
        let (alice, bob) = tokio::join!(ctx.login("alice"), ctx.login("bob"));
        let (calendar, mut joined) = shared_calendar(&ctx, &alice, &[&bob], false).await;
        let room_id = room_id_of(&calendar);
        let bob_calendar = joined.pop().expect("bob's calendar");
        assert!(
            list_calendars(&bob.client)
                .iter()
                .any(|c| c.room_id() == room_id)
        );

        bob_calendar.leave().await.expect("leave the calendar");

        assert!(
            !list_calendars(&bob.client)
                .iter()
                .any(|c| c.room_id() == room_id)
        );
        assert!(!has_invitation(&bob, &room_id));
        bob.sync_once().await;
        assert!(
            !list_calendars(&bob.client)
                .iter()
                .any(|c| c.room_id() == room_id)
        );
        if let Some(room) = bob.client.get_room(&room_id) {
            assert_eq!(room.state(), RoomState::Left);
        }

        // Alice keeps the calendar; bob is no longer a member.
        assert!(
            list_calendars(&alice.client)
                .iter()
                .any(|c| c.room_id() == room_id)
        );
        alice
            .sync_until_some("bob to be gone from the member list", async || {
                roles_seen_by(&calendar)
                    .await
                    .iter()
                    .all(|(user, _, _)| *user != bob.user_id)
                    .then_some(())
            })
            .await;

        // Alice can re-share the calendar, and it arrives as a new invitation.
        calendar.invite_editor(&bob.user_id).await.unwrap();
        let invitation = wait_for_invitation(&bob, &room_id).await;
        assert_eq!(invitation.inviter().await.as_deref(), Some(&*alice.user_id));
    })
    .await;
}
