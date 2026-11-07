//! The Stage hook of the hub: what happens when the owner says "@stage golden hour" (or says
//! those words to "@tempo"). See `stage_hook.rs`. The owner's words are stored like any message;
//! everything Stage says back to them is private (owner only), because a path on the owner's
//! disk is nobody else's business. One plan waits for a y or n in each room for ten minutes.

use super::*;
use crate::stage_hook::{hook_of, Hook, StageRunner};

const WAIT: Duration = Duration::from_secs(600);

pub struct Pending {
    phrase: String,
    at: Instant,
}

impl Hub {
    fn stage_runner(&self) -> Arc<dyn StageRunner> {
        lock(&self.stage).clone()
    }

    /// Use another runner (tests, and a Room started beside a launcher somewhere else).
    pub fn set_stage_runner(&self, r: Arc<dyn StageRunner>) {
        *lock(&self.stage) = r;
    }

    fn stage_pending_in(&self, room: &str) -> bool {
        let mut g = lock(&self.stage_pending);
        g.retain(|_, p| p.at.elapsed() < WAIT);
        g.contains_key(room)
    }

    fn me(&self) -> Option<Arc<Hub>> {
        lock(&self.me).upgrade()
    }

    /// Says something to the owner only.
    fn stage_say(&self, room: &str, text: &str, reply_to: Option<u64>, via: &str) {
        self.post_tempo_as(room, text, reply_to, via, None, true);
    }

    pub(super) fn stage_hook(
        &self,
        r: &PostReq,
        text: &str,
        handles: &[String],
    ) -> Option<Result<Posted, Refusal>> {
        let owner_ok = r.who.owner && r.owner_session.is_some() && matches!(r.via, "app" | "cli");
        let hook = hook_of(text, handles, owner_ok, self.stage_pending_in(r.room))?;
        if let Hook::Answer(yes) = hook {
            return Some(self.stage_answer(r, yes));
        }
        // the words are a message in the room like any other
        let m = Msg::new(
            r.room,
            &r.who.from,
            &r.who.name,
            Kind::User,
            r.via,
            &scrub(text),
        );
        let m = match self.store_msg(m) {
            Ok(m) => m,
            Err(e) => return Some(Err(e)),
        };
        match hook {
            Hook::Refuse => {
                self.post_tempo(
                    r.room,
                    "Only the owner can ask Stage for things, from their own computer. Ask them, and they can say it here.",
                    Some(m.id),
                    r.via,
                    r.target.as_ref(),
                );
            }
            Hook::Cue(phrase) => self.stage_plan(r, &m, phrase),
            Hook::Answer(_) => {}
        }
        Some(Ok(Posted::Msg(m)))
    }

    /// Ask Stage what the words mean, then ask the owner.
    fn stage_plan(&self, r: &PostReq, m: &Msg, phrase: String) {
        let (room, via, id) = (r.room.to_string(), r.via.to_string(), m.id);
        let Some(hub) = self.me() else { return };
        if lock(&self.stage_busy).contains(&room) {
            return self.stage_say(
                &room,
                "Stage is still playing the last one. Ask again when it is done.",
                Some(id),
                &via,
            );
        }
        std::thread::spawn(move || match hub.stage_runner().plan(&phrase) {
            Ok(plan) => {
                lock(&hub.stage_pending).insert(
                    room.clone(),
                    Pending {
                        phrase,
                        at: Instant::now(),
                    },
                );
                hub.stage_say(
                    &room,
                    &format!("Stage heard: {plan}\nPlay it? Say y or n here."),
                    Some(id),
                    &via,
                );
            }
            Err(e) => hub.stage_say(&room, &e, Some(id), &via),
        });
    }

    /// The owner's y or n.
    fn stage_answer(&self, r: &PostReq, yes: bool) -> Result<Posted, Refusal> {
        let Some(p) = lock(&self.stage_pending).remove(r.room) else {
            return Ok(Posted::Note("Nothing is waiting for a y or n.".into()));
        };
        if !yes {
            return Ok(Posted::Note("Left it. Nothing was played.".into()));
        }
        let Some(hub) = self.me() else {
            return Ok(Posted::Note("Room is shutting down.".into()));
        };
        let (room, via) = (r.room.to_string(), r.via.to_string());
        lock(&self.stage_busy).insert(room.clone());
        std::thread::spawn(move || {
            let r = hub.stage_runner().play(&p.phrase);
            lock(&hub.stage_busy).remove(&room);
            match r {
                Ok(done) => hub.stage_say(&room, &done.text, None, &via),
                Err(e) => {
                    hub.stage_say(&room, &format!("Stage could not play it: {e}"), None, &via)
                }
            }
        });
        Ok(Posted::Note(
            "Playing it. I'll tell you here when it is done; it can take minutes.".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{hub_at, member, say, wait_msg};
    use super::*;
    use crate::stage_hook::Played;
    use crate::testutil::FakeAgent;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A Stage that writes down what it was asked.
    struct FakeStage {
        planned: Mutex<Vec<String>>,
        played: Mutex<Vec<String>>,
        gate: AtomicUsize,
    }

    impl FakeStage {
        fn new() -> Arc<FakeStage> {
            Arc::new(FakeStage {
                planned: Mutex::new(vec![]),
                played: Mutex::new(vec![]),
                gate: AtomicUsize::new(0),
            })
        }
    }

    impl StageRunner for FakeStage {
        fn plan(&self, phrase: &str) -> Result<String, String> {
            lock(&self.planned).push(phrase.to_string());
            if phrase.contains("nothing") {
                return Err("I did not catch a Pedal or a workflow in that.".into());
            }
            Ok(format!("Re-angle (re-angle-video) with camera=orbit_left, on the last run's inputs. It plays on the studio box, about 41 s. [{phrase}]"))
        }
        fn play(&self, phrase: &str) -> Result<Played, String> {
            lock(&self.played).push(phrase.to_string());
            self.gate.fetch_add(1, Ordering::SeqCst);
            Ok(Played {
                text: "Re-angled on the studio box, 41 s. Result: /home/owner/work/a.mp4".into(),
                files: vec!["/home/owner/work/a.mp4".into()],
            })
        }
    }

    fn owner(hub: &Hub, text: &str) -> Result<Posted, Refusal> {
        let mut r = PostReq::new("family", Speaker::owner("Riley"), text, "app");
        r.owner_session = Some("s1".into());
        hub.post(r)
    }

    fn private_tempo(hub: &Hub, needle: &str) -> Msg {
        wait_msg(hub, needle, |m| {
            m.kind == Kind::Tempo && m.owner_only && m.text.contains(needle)
        })
    }

    #[test]
    fn the_owner_says_at_stage_and_is_asked_y_n_then_it_plays_and_the_path_comes_back_privately() {
        let agent = FakeAgent::start(vec![]);
        let (hub, _d) = hub_at(&agent.url(), "stage-ok", json!({"owner_name": "Riley"}));
        let fake = FakeStage::new();
        hub.set_stage_runner(fake.clone());
        let said = owner(&hub, "@stage re-angle the last clip with a slow orbit");
        assert!(matches!(said, Ok(Posted::Msg(_))));
        // it asks, in private, and has not played
        let ask = private_tempo(&hub, "Stage heard:");
        assert!(
            ask.text.contains("camera=orbit_left") && ask.text.contains("Say y or n"),
            "{}",
            ask.text
        );
        assert!(lock(&fake.played).is_empty());
        assert_eq!(
            lock(&fake.planned).as_slice(),
            ["re-angle the last clip with a slow orbit"]
        );
        // y: it plays, and the result path is posted to the owner only
        let r = owner(&hub, "y");
        assert!(
            matches!(r, Ok(Posted::Note(ref n)) if n.contains("Playing it")),
            "{r:?}"
        );
        let done = private_tempo(&hub, "Result: /home/owner/work/a.mp4");
        assert!(done.owner_only);
        assert_eq!(lock(&fake.played).len(), 1);
        // the owner's y never went into the room as a message, and the path is in no public message
        let room = hub.lock().room("family").unwrap().tail.clone();
        assert!(room
            .iter()
            .all(|m| m.owner_only || !m.text.contains("/home/owner")));
        assert!(!room.iter().any(|m| m.text == "y"));
        hub.shutdown();
    }

    #[test]
    fn n_plays_nothing_and_a_y_with_nothing_waiting_is_just_a_message() {
        let agent = FakeAgent::start(vec![]);
        let (hub, _d) = hub_at(&agent.url(), "stage-no", json!({"owner_name": "Riley"}));
        let fake = FakeStage::new();
        hub.set_stage_runner(fake.clone());
        // nothing waits: "y" is an ordinary message
        assert!(matches!(owner(&hub, "y"), Ok(Posted::Msg(_))));
        owner(&hub, "@stage golden hour").unwrap();
        private_tempo(&hub, "Stage heard:");
        let r = owner(&hub, "n");
        assert!(
            matches!(r, Ok(Posted::Note(ref n)) if n.contains("Nothing was played")),
            "{r:?}"
        );
        assert!(lock(&fake.played).is_empty());
        // and the plan is gone: a late y plays nothing
        assert!(matches!(owner(&hub, "y"), Ok(Posted::Msg(_))));
        assert!(lock(&fake.played).is_empty());
        // words Stage cannot read are told so, and nothing waits for a y
        owner(&hub, "@stage nothing at all").unwrap();
        private_tempo(&hub, "did not catch");
        assert!(matches!(owner(&hub, "y"), Ok(Posted::Msg(_))));
        assert!(lock(&fake.played).is_empty());
        hub.shutdown();
    }

    #[test]
    fn a_member_cannot_ask_stage_or_answer_for_the_owner() {
        let agent = FakeAgent::start(vec![]);
        let (hub, _d) = hub_at(&agent.url(), "stage-guest", json!({"owner_name": "Riley"}));
        let fake = FakeStage::new();
        hub.set_stage_runner(fake.clone());
        let mia = member(&hub, "Mia");
        let m = say(&hub, &mia, "@stage golden hour").unwrap();
        let Posted::Msg(m) = m else { panic!("stored") };
        let reply = wait_msg(&hub, "the refusal", |x| {
            x.kind == Kind::Tempo && x.reply_to == Some(m.id)
        });
        assert!(reply.text.contains("Only the owner"), "{}", reply.text);
        assert!(lock(&fake.planned).is_empty() && lock(&fake.played).is_empty());
        // the owner has a plan waiting; Mia's "y" is only a message
        owner(&hub, "@stage golden hour").unwrap();
        private_tempo(&hub, "Stage heard:");
        assert!(matches!(say(&hub, &mia, "y"), Ok(Posted::Msg(_))));
        // and a member's "@tempo golden hour" is an ordinary ask: no plan, no play
        let before = lock(&fake.planned).len();
        say(&hub, &mia, "@tempo golden hour").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(lock(&fake.planned).len(), before);
        assert!(lock(&fake.played).is_empty());
        // from outside the owner's own session (a bridge, a link without the cookie) the owner's
        // name is not enough
        let mut r = PostReq::new("family", Speaker::owner("Riley"), "@stage noir", "app");
        r.owner_session = None;
        let Ok(Posted::Msg(m)) = hub.post(r) else {
            panic!("stored")
        };
        let reply = wait_msg(&hub, "the refusal", |x| {
            x.kind == Kind::Tempo && x.reply_to == Some(m.id)
        });
        assert!(reply.text.contains("Only the owner"));
        hub.shutdown();
    }

    #[test]
    fn the_same_words_said_to_tempo_by_the_owner_reach_stage_and_a_question_does_not() {
        let agent = FakeAgent::start(vec![crate::testutil::Script::Answer(
            "It is the hour after sunrise.".into(),
        )]);
        let (hub, _d) = hub_at(&agent.url(), "stage-tempo", json!({"owner_name": "Riley"}));
        let fake = FakeStage::new();
        hub.set_stage_runner(fake.clone());
        owner(&hub, "@tempo make it golden hour").unwrap();
        private_tempo(&hub, "Stage heard:");
        assert_eq!(lock(&fake.planned).as_slice(), ["make it golden hour"]);
        owner(&hub, "n").unwrap();
        // a question that happens to say golden hour goes to Tempo as a question
        let Ok(Posted::Msg(q)) = owner(&hub, "@tempo what is golden hour?") else {
            panic!("stored")
        };
        let a = wait_msg(&hub, "Tempo's answer", |x| {
            x.kind == Kind::Tempo && x.reply_to == Some(q.id)
        });
        assert!(a.text.contains("sunrise"), "{}", a.text);
        assert_eq!(lock(&fake.planned).len(), 1);
        hub.shutdown();
    }

    #[test]
    fn one_play_at_a_time_in_a_room() {
        let agent = FakeAgent::start(vec![]);
        let (hub, _d) = hub_at(&agent.url(), "stage-busy", json!({"owner_name": "Riley"}));
        let fake = FakeStage::new();
        hub.set_stage_runner(fake.clone());
        lock(&hub.stage_busy).insert("family".into());
        owner(&hub, "@stage golden hour").unwrap();
        private_tempo(&hub, "still playing");
        assert!(lock(&fake.planned).is_empty());
        hub.shutdown();
    }
}
