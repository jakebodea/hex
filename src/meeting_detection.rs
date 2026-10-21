use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::context::ContextSnapshot;
use crate::microphone_activity::ActiveMicrophoneApplication;

const STABLE_OBSERVATIONS: u8 = 4;
/// An offered call stays snoozed until its app has been off the microphone
/// for about a minute of 500 ms observations, so one call is offered once.
const ABSENT_OBSERVATIONS_TO_RESET: u8 = 120;
/// About three seconds without the call app on the microphone.
const CALL_ENDED_OBSERVATIONS: u8 = 6;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MeetingCandidate {
    pub key: String,
    pub title: String,
    pub source: MeetingSource,
    /// Canonical bundle identifier of the application using the microphone.
    #[serde(default)]
    pub application: Option<String>,
    /// Whether `title` names this meeting rather than its app.
    #[serde(default)]
    pub named: bool,
}

impl MeetingCandidate {
    pub fn manual() -> Self {
        Self {
            key: "manual".into(),
            title: MeetingSource::Manual.label().into(),
            source: MeetingSource::Manual,
            application: None,
            named: false,
        }
    }

    pub fn recording_start(&self) -> crate::meeting::MeetingStart {
        crate::meeting::MeetingStart {
            title: self.named.then(|| self.title.clone()),
            app: (self.source != MeetingSource::Manual).then(|| self.source.label().into()),
        }
    }
}

/// Notices when the call application behind a recording stops using the
/// microphone, so the user can choose to stop. It never stops anything.
#[derive(Default)]
pub struct CallPresence {
    absent: u8,
    prompted: bool,
}

impl CallPresence {
    /// Returns true once per absence that lasts long enough.
    pub fn observe(&mut self, present: bool) -> bool {
        if present {
            self.absent = 0;
            self.prompted = false;
            return false;
        }
        self.absent = self.absent.saturating_add(1);
        if self.absent >= CALL_ENDED_OBSERVATIONS && !self.prompted {
            self.prompted = true;
            return true;
        }
        false
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingSource {
    Manual,
    GoogleMeet,
    Zoom,
    Teams,
    SlackHuddle,
    FaceTime,
    BrowserCall,
}

impl MeetingSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "Meeting",
            Self::GoogleMeet => "Google Meet",
            Self::Zoom => "Zoom",
            Self::Teams => "Microsoft Teams",
            Self::SlackHuddle => "Slack Huddle",
            Self::FaceTime => "FaceTime",
            Self::BrowserCall => "Browser call",
        }
    }
}

#[derive(Default)]
pub struct MeetingDetector {
    pending: Option<(String, u8)>,
    offered: Option<String>,
    absent_observations: u8,
}

impl MeetingDetector {
    pub fn observe(&mut self, candidate: Option<MeetingCandidate>) -> Option<MeetingCandidate> {
        let Some(candidate) = candidate else {
            self.pending = None;
            self.absent_observations = self.absent_observations.saturating_add(1);
            if self.absent_observations >= ABSENT_OBSERVATIONS_TO_RESET {
                self.offered = None;
            }
            return None;
        };
        self.absent_observations = 0;
        if self.offered.as_deref() == Some(&candidate.key) {
            return None;
        }
        let observations = match &mut self.pending {
            Some((key, count)) if key == &candidate.key => {
                *count = count.saturating_add(1);
                *count
            }
            _ => {
                self.pending = Some((candidate.key.clone(), 1));
                1
            }
        };
        if observations < STABLE_OBSERVATIONS {
            return None;
        }
        self.pending = None;
        self.offered = Some(candidate.key.clone());
        Some(candidate)
    }
}

pub fn candidate_from_context(context: &ContextSnapshot) -> Option<MeetingCandidate> {
    let application = context.application.as_deref()?;
    let application_lower = application.to_ascii_lowercase();
    let title = context.window_title.as_deref().unwrap_or("").trim();
    let title_lower = title.to_ascii_lowercase();

    if application_lower.contains("brave")
        && let Some(url) = &context.browser_url
        && is_google_meet_call(url)
    {
        return Some(named(candidate(
            MeetingSource::GoogleMeet,
            url.as_str(),
            clean_title(title, &[" - Google Meet", " | Google Meet"]),
        )));
    }
    if application_lower.contains("zoom")
        && (title_lower.contains("meeting") || title_lower.contains("webinar"))
    {
        return Some(candidate(MeetingSource::Zoom, title, title));
    }
    if application_lower.contains("teams")
        && (title_lower.contains("meeting") || title_lower.contains("call"))
    {
        return Some(candidate(MeetingSource::Teams, title, title));
    }
    if application_lower == "slack" && title_lower.contains("huddle") {
        return Some(candidate(MeetingSource::SlackHuddle, title, title));
    }
    if application_lower == "facetime" && !title.is_empty() && title_lower != "facetime" {
        return Some(named(candidate(MeetingSource::FaceTime, title, title)));
    }
    None
}

fn named(mut candidate: MeetingCandidate) -> MeetingCandidate {
    candidate.named = candidate.title != candidate.source.label();
    candidate
}

pub fn candidate_from_microphone(
    application: &ActiveMicrophoneApplication,
    context: &ContextSnapshot,
) -> Option<MeetingCandidate> {
    let canonical_bundle = application.canonical_bundle_id()?;
    if application.is_browser() {
        if context
            .application
            .as_deref()
            .is_some_and(|foreground| foreground.contains(&application.name))
            && let Some(mut candidate) = candidate_from_context(context)
        {
            candidate.application = Some(canonical_bundle.into());
            return Some(candidate);
        }
        let mut candidate = candidate(MeetingSource::BrowserCall, canonical_bundle, "Browser call");
        candidate.application = Some(canonical_bundle.into());
        return Some(candidate);
    }
    let (source, title) = match canonical_bundle {
        "us.zoom.xos" => (MeetingSource::Zoom, "Zoom meeting"),
        "com.microsoft.teams2" => (MeetingSource::Teams, "Microsoft Teams meeting"),
        "com.tinyspeck.slackmacgap" => (MeetingSource::SlackHuddle, "Slack huddle"),
        "com.apple.FaceTime" => (MeetingSource::FaceTime, "FaceTime call"),
        _ => return None,
    };
    let mut candidate = candidate(source, canonical_bundle, title);
    candidate.application = Some(canonical_bundle.into());
    Some(candidate)
}

fn candidate(source: MeetingSource, identity: &str, title: &str) -> MeetingCandidate {
    MeetingCandidate {
        key: opaque_key(source, identity),
        title: if title.is_empty() {
            source.label().into()
        } else {
            title.into()
        },
        source,
        application: None,
        named: false,
    }
}

fn opaque_key(source: MeetingSource, identity: &str) -> String {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    identity.hash(&mut hasher);
    format!("meeting-{:016x}", hasher.finish())
}

fn is_google_meet_call(url: &Url) -> bool {
    if url.host_str() != Some("meet.google.com") {
        return false;
    }
    let path = url.path().trim_matches('/');
    !path.is_empty() && path != "landing" && path != "new"
}

fn clean_title<'a>(title: &'a str, suffixes: &[&str]) -> &'a str {
    suffixes
        .iter()
        .find_map(|suffix| title.strip_suffix(suffix))
        .unwrap_or(title)
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(application: &str, url: Option<&str>, title: &str) -> ContextSnapshot {
        ContextSnapshot {
            application: Some(application.into()),
            bundle_id: None,
            browser_url: url.map(|url| Url::parse(url).unwrap()),
            window_title: Some(title.into()),
            selected_text: None,
            input_revision: None,
        }
    }

    #[test]
    fn recognizes_calls_without_treating_landing_pages_as_meetings() {
        assert!(
            candidate_from_context(&context(
                "Brave Browser",
                Some("https://meet.google.com/abc-defg-hij"),
                "Design sync - Google Meet"
            ))
            .is_some()
        );
        assert!(
            candidate_from_context(&context(
                "Brave Browser",
                Some("https://meet.google.com/landing"),
                "Google Meet"
            ))
            .is_none()
        );
        assert!(candidate_from_context(&context("zoom.us", None, "Zoom Workplace")).is_none());
        assert!(candidate_from_context(&context("zoom.us", None, "Zoom Meeting")).is_some());
    }

    #[test]
    fn requires_stability_and_offers_once_per_session() {
        let candidate = candidate_from_context(&context(
            "Brave Browser",
            Some("https://meet.google.com/abc-defg-hij"),
            "Design sync - Google Meet",
        ));
        let mut detector = MeetingDetector::default();
        assert!(detector.observe(candidate.clone()).is_none());
        assert!(detector.observe(candidate.clone()).is_none());
        assert!(detector.observe(candidate.clone()).is_none());
        assert!(detector.observe(candidate.clone()).is_some());
        assert!(detector.observe(candidate.clone()).is_none());
        for _ in 0..ABSENT_OBSERVATIONS_TO_RESET {
            assert!(detector.observe(None).is_none());
        }
        assert!(detector.observe(candidate.clone()).is_none());
        assert!(detector.observe(candidate.clone()).is_none());
        assert!(detector.observe(candidate.clone()).is_none());
        assert!(detector.observe(candidate).is_some());
    }

    #[test]
    fn a_brief_microphone_release_does_not_re_offer_the_same_call() {
        let candidate = candidate(MeetingSource::Zoom, "us.zoom.xos", "Zoom meeting");
        let mut detector = MeetingDetector::default();
        for _ in 0..STABLE_OBSERVATIONS - 1 {
            assert!(detector.observe(Some(candidate.clone())).is_none());
        }
        assert!(detector.observe(Some(candidate.clone())).is_some());
        // Dismissed or ignored: twenty seconds off the microphone keeps it snoozed.
        for _ in 0..40 {
            assert!(detector.observe(None).is_none());
        }
        for _ in 0..10 {
            assert!(detector.observe(Some(candidate.clone())).is_none());
        }
    }

    #[test]
    fn named_meetings_keep_their_title_and_app_calls_use_the_app() {
        let meet = candidate_from_context(&context(
            "Brave Browser",
            Some("https://meet.google.com/abc-defg-hij"),
            "Design sync - Google Meet",
        ))
        .unwrap();
        let start = meet.recording_start();
        assert_eq!(start.title.as_deref(), Some("Design sync"));
        assert_eq!(start.app.as_deref(), Some("Google Meet"));

        let zoom = candidate(MeetingSource::Zoom, "us.zoom.xos", "Zoom meeting").recording_start();
        assert_eq!(zoom.title, None);
        assert_eq!(zoom.app.as_deref(), Some("Zoom"));

        let manual = MeetingCandidate::manual().recording_start();
        assert_eq!((manual.title, manual.app), (None, None));
    }

    #[test]
    fn call_end_is_reported_once_per_sustained_absence() {
        let mut presence = CallPresence::default();
        assert!(!presence.observe(true));
        for _ in 0..CALL_ENDED_OBSERVATIONS - 1 {
            assert!(!presence.observe(false));
        }
        assert!(presence.observe(false));
        assert!(!presence.observe(false));
        // Rejoining and leaving again asks again.
        assert!(!presence.observe(true));
        for _ in 0..CALL_ENDED_OBSERVATIONS - 1 {
            assert!(!presence.observe(false));
        }
        assert!(presence.observe(false));
    }
}
