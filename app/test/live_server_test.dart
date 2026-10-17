// Runs only against a real server: AGENTCORE_TEST_URL and AGENTCORE_TEST_TOKEN,
// with the `demo` agent from agentcore.dev.toml.
import 'dart:io';

import 'package:agentcore_app/api/client.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  final url = Platform.environment['AGENTCORE_TEST_URL'];
  test('streams a session over SSE and approves its action', () async {
    final client = ApiClient(server: parseServerUrl(url!)!, token: Platform.environment['AGENTCORE_TEST_TOKEN']);
    final session = await client.createSession(agent: 'demo', task: 'SSE check');
    final kinds = <String>[];
    await for (final e in client.sessionEvents(session.id).timeout(const Duration(seconds: 30))) {
      kinds.add(e.kind);
      if (e.kind == 'approval_requested') {
        await client.decide(session.id, e.raw['approval_id'] as String, approved: false, comment: 'not now');
      }
      if (e.kind == 'session_ended') break;
    }
    expect(kinds, containsAllInOrder(['session_created', 'approval_requested', 'approval_resolved', 'session_ended']));
    client.close();
  }, skip: url == null ? 'set AGENTCORE_TEST_URL to run against a server' : false);
}
