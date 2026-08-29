//! Holding the system's default devices on ours.
//!
//! Voicemeeter's virtual cables are only useful if applications actually
//! reach them, and on a desktop that means being the default sink and
//! source. Something else moves those all the time - plugging in a
//! headset, a session manager deciding it knows better - so this both
//! sets them and puts them back.
//!
//! Through `pw-metadata` rather than the metadata API: the same route
//! `wpctl set-default` takes, it is the documented way to express a
//! *configured* default rather than a guessed one, and this program
//! already shells out to `pipewire` for its filter chains.

use std::process::Command;

/// The keys `PipeWire` keeps the configured defaults under.
///
/// `configured` rather than plain `default.audio.sink`: the plain one is
/// what the session manager worked out, and writing it is a suggestion
/// that gets overwritten. The configured one is a person's choice, which
/// is what a ticked box in a menu is.
const SINK_KEY: &str = "default.configured.audio.sink";
const SOURCE_KEY: &str = "default.configured.audio.source";

/// What the defaults were before we touched them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Saved {
    pub sink: Option<String>,
    pub source: Option<String>,
}

/// Read the node name currently set for a key, if any.
#[must_use]
pub fn configured(key: &str) -> Option<String> {
    let out = Command::new("pw-metadata")
        .args(["-n", "default", key])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_name(&text, key)
}

/// Pull one key's node name out of `pw-metadata`'s report.
///
/// Asking for a key does *not* narrow the output: it prints every key it
/// has, and the first `"name"` in that dump belongs to
/// `default.audio.sink` whatever was asked for. So the line has to be
/// found by its key first - reading the first name would have answered
/// every question with the sink's value.
///
/// Its own function so it can be tested without a running `PipeWire`.
#[must_use]
pub fn parse_name(text: &str, key: &str) -> Option<String> {
    let needle = format!("key:'{key}'");
    let line = text.lines().find(|line| line.contains(&needle))?;
    let at = line.find("\"name\"")?;
    let rest = &line[at + 6..];
    let open = rest.find('"')?;
    let tail = &rest[open + 1..];
    let close = tail.find('"')?;
    let name = &tail[..close];
    (!name.is_empty()).then(|| name.to_owned())
}

/// What both defaults are right now, to be put back later.
#[must_use]
pub fn snapshot() -> Saved {
    Saved {
        sink: configured(SINK_KEY),
        source: configured(SOURCE_KEY),
    }
}

/// Point a key at a node, or clear it when given nothing.
///
/// The `0` is the subject id and is not optional: without it
/// `pw-metadata` reads the key as the id, reports the write in its own
/// output, and changes nothing. The log said it had worked for as long as
/// that argument was missing.
///
/// Clearing matters for the restore: a machine that had no *configured*
/// default before should go back to having none, rather than being left
/// pinned to whatever happened to be in use when the mixer started.
pub fn set(key: &str, node: Option<&str>) {
    let value = node.map_or_else(|| "null".to_owned(), |n| format!("{{ \"name\": \"{n}\" }}"));
    let result = Command::new("pw-metadata")
        .args(["-n", "default", "0", key, &value])
        .output();
    match result {
        Ok(out) if out.status.success() => log::debug!("default {key} -> {value}"),
        Ok(out) => log::warn!(
            "could not set {key}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(err) => log::warn!("could not run pw-metadata: {err}"),
    }
}

/// Make our devices the defaults.
pub fn claim(sink: &str, source: &str) {
    set(SINK_KEY, Some(sink));
    set(SOURCE_KEY, Some(source));
}

/// Put back what was there before.
pub fn restore(saved: &Saved) {
    set(SINK_KEY, saved.sink.as_deref());
    set(SOURCE_KEY, saved.source.as_deref());
}

/// Whether the defaults still point at ours.
#[must_use]
pub fn held(sink: &str, source: &str) -> bool {
    configured(SINK_KEY).as_deref() == Some(sink)
        && configured(SOURCE_KEY).as_deref() == Some(source)
}

#[cfg(test)]
mod tests {
    use super::parse_name;

    /// A real dump, in the order pw-metadata prints it. Asking for a key
    /// does not narrow the output, so the plain sink line comes first
    /// whatever was requested.
    const DUMP: &str = "Found \"default\" metadata 51\n\
update: id:0 key:'default.audio.sink' value:'{\"name\":\"wivrn.sink\"}' type:'Spa:String:JSON'\n\
update: id:0 key:'default.audio.source' value:'{\"name\":\"wivrn.source\"}' type:'Spa:String:JSON'\n\
update: id:0 key:'default.configured.audio.sink' value:'{ \"name\": \"pipemeter_vaio\" }' type:'Spa:String:JSON'\n\
update: id:0 key:'default.configured.audio.source' value:'{ \"name\": \"pipemeter_b1\" }' type:'Spa:String:JSON'\n";

    #[test]
    fn each_key_reads_its_own_value() {
        assert_eq!(
            parse_name(DUMP, "default.configured.audio.sink").as_deref(),
            Some("pipemeter_vaio")
        );
        assert_eq!(
            parse_name(DUMP, "default.configured.audio.source").as_deref(),
            Some("pipemeter_b1")
        );
    }

    /// The bug this guards: taking the first name in the dump answers
    /// every question with `default.audio.sink`.
    #[test]
    fn the_source_is_not_answered_with_the_sink() {
        let source = parse_name(DUMP, "default.configured.audio.source");
        assert_ne!(source.as_deref(), Some("wivrn.sink"));
    }

    #[test]
    fn nothing_configured_reads_as_nothing() {
        assert_eq!(parse_name("", "any"), None);
        assert_eq!(parse_name(DUMP, "default.configured.video.source"), None);
    }
}
