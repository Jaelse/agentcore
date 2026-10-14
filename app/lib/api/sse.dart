import 'dart:async';
import 'dart:convert';

/// One server-sent event.
class SseMessage {
  const SseMessage(this.event, this.data);
  final String event;
  final String data;
}

/// Splits a text/event-stream body into messages (the subset agentcore
/// sends: `event:`, `data:`, `id:` and comment lines).
class SseDecoder extends StreamTransformerBase<String, SseMessage> {
  const SseDecoder();

  @override
  Stream<SseMessage> bind(Stream<String> stream) async* {
    var event = 'message';
    final data = <String>[];
    await for (final line in stream.transform(const LineSplitter())) {
      if (line.isEmpty) {
        if (data.isNotEmpty) yield SseMessage(event, data.join('\n'));
        event = 'message';
        data.clear();
      } else if (line.startsWith('event:')) {
        event = line.substring(6).trim();
      } else if (line.startsWith('data:')) {
        final value = line.substring(5);
        data.add(value.startsWith(' ') ? value.substring(1) : value);
      }
      // `id:`, `retry:` and `:` comments (keep-alives) carry nothing we need.
    }
    if (data.isNotEmpty) yield SseMessage(event, data.join('\n'));
  }
}
