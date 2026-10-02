//! Minimal Server-Sent Events decoder (the subset the API emits).

/// One complete server-sent event.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Event {
    /// The `event` field; empty when the server sent none.
    pub name: String,
    /// The `data` field, multi-line data joined with `\n`.
    pub data: String,
}

/// Accumulates SSE lines and yields complete events.
///
/// SereChat names each event in the `event` field and leaves `type` out of
/// the JSON payload, so both fields are kept. `id` and `retry` are ignored.
/// MCP servers stream with the same format.
#[derive(Debug, Default)]
pub struct Decoder {
    event: Event,
    has_data: bool,
}

impl Decoder {
    /// Feeds one line (without its terminator). Returns the event when the
    /// line completes one.
    pub fn line(&mut self, line: &str) -> Option<Event> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            return std::mem::take(&mut self.has_data).then_some(event);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => value.clone_into(&mut self.event.name),
            "data" => {
                if self.has_data {
                    self.event.data.push('\n');
                }
                self.event.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoder, Event};

    #[test]
    fn decodes_events() {
        let mut d = Decoder::default();
        let stream = "event: x\r\ndata: {\"a\":1}\r\n\r\n: comment\ndata:one\ndata: two\n\n\ndata: tail";
        let events: Vec<_> = stream.lines().chain([""]).filter_map(|l| d.line(l)).collect();
        let event = |name: &str, data: &str| Event { name: name.into(), data: data.into() };
        assert_eq!(events, [event("x", "{\"a\":1}"), event("", "one\ntwo"), event("", "tail")]);
    }
}
