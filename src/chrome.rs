//! Google Chrome specifics: recognising Chrome processes and parsing the
//! profile out of window titles (`Tab title - Google Chrome - Profile (email)`).

/// Exact application names we manage. Helper processes ("Google Chrome
/// Helper (Renderer)") are deliberately excluded.
const APPS: [&str; 5] = ["Google Chrome", "Google Chrome Beta", "Google Chrome Canary", "Google Chrome Dev", "Chromium"];

pub fn is_chrome(owner: &str) -> bool {
    APPS.contains(&owner)
}

/// Accessibility appends tab annotations to the window title that the visible
/// title bar does not show.
fn is_annotation(segment: &str) -> bool {
    const PREFIXES: [&str; 6] =
        ["Part of group ", "High memory usage", "Playing audio", "Audio muted", "Camera or microphone", "Notification"];
    PREFIXES.iter().any(|p| segment.starts_with(p)) || segment.ends_with(" GB") || segment.ends_with(" MB")
}

/// Splits `Tab title - Google Chrome - Profile (email)` into
/// (tab title, profile). Segments are separated by " - " or " – ".
pub fn split_title(full: &str, app: &str) -> (String, Option<String>) {
    let parts: Vec<&str> = full
        .split(" - ")
        .flat_map(|p| p.split(" – "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let Some(i) = parts.iter().rposition(|p| *p == app) else {
        return (full.trim().to_string(), None);
    };
    let title: Vec<&str> = parts[..i].iter().copied().filter(|s| !is_annotation(s)).collect();
    let title = if title.is_empty() { app.to_string() } else { title.join(" - ") };
    let profile = parts[i + 1..].join(" - ");
    // "Agency Collective (team@example.com)" -> "Agency Collective"
    let profile = match profile.rfind(" (") {
        Some(p) if profile.ends_with(')') => profile[..p].to_string(),
        _ => profile,
    };
    (title, (!profile.is_empty()).then_some(profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_with_email() {
        assert_eq!(
            split_title("Inbox (3) - Gmail - Google Chrome - Work (me@example.com)", "Google Chrome"),
            ("Inbox (3) - Gmail".into(), Some("Work".into()))
        );
    }

    #[test]
    fn profile_without_email_and_en_dash() {
        assert_eq!(split_title("Docs - Google Chrome – Konstantin", "Google Chrome"), ("Docs".into(), Some("Konstantin".into())));
    }

    #[test]
    fn single_profile_has_no_suffix() {
        assert_eq!(split_title("Docs - Google Chrome", "Google Chrome"), ("Docs".into(), None));
    }

    #[test]
    fn tab_title_equal_to_app_name() {
        assert_eq!(split_title("Google Chrome - Google Chrome - Work", "Google Chrome"), ("Google Chrome".into(), Some("Work".into())));
    }

    #[test]
    fn strips_accessibility_annotations() {
        let t = "Inbox - Mail - Part of group Team - High memory usage - 1.1 GB - Google Chrome - Agency (a@b.c)";
        assert_eq!(split_title(t, "Google Chrome"), ("Inbox - Mail".into(), Some("Agency".into())));
    }

    #[test]
    fn no_app_segment_means_no_profile() {
        assert_eq!(split_title("Konstantin - Google Search", "Google Chrome"), ("Konstantin - Google Search".into(), None));
        assert_eq!(split_title("", "Google Chrome"), (String::new(), None));
    }

    #[test]
    fn helpers_are_not_chrome() {
        assert!(is_chrome("Google Chrome"));
        assert!(!is_chrome("Google Chrome Helper (Renderer)"));
    }
}
