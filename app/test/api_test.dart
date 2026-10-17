import 'dart:convert';

import 'package:agentcore_app/api/client.dart';
import 'package:agentcore_app/api/models.dart';
import 'package:agentcore_app/api/sse.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

void main() {
  group('parseServerUrl', () {
    test('adds https and drops trailing slashes', () {
      expect(parseServerUrl('agentcore.example.com/')!.toString(), 'https://agentcore.example.com');
      expect(parseServerUrl(' http://10.0.2.2:8080 ')!.toString(), 'http://10.0.2.2:8080');
      expect(parseServerUrl('https://example.com/agentcore//')!.toString(), 'https://example.com/agentcore');
    });

    test('rejects what is not a web address', () {
      expect(parseServerUrl(''), isNull);
      expect(parseServerUrl('ftp://example.com'), isNull);
      expect(parseServerUrl('https://'), isNull);
    });
  });

  test('SseDecoder splits events, joins data lines and skips comments', () async {
    final chunks = Stream.fromIterable([
      ': keep-alive\n\nevent: event\nid: 1\ndata: {"a":',
      '1}\n\nevent: lagged\ndata: x\ndata: y\n\n',
      'data: tail',
    ]);
    final messages = await chunks.transform(const SseDecoder()).toList();
    expect(messages.map((m) => m.event), ['event', 'lagged', 'message']);
    expect(messages.map((m) => m.data), ['{"a":1}', 'x\ny', 'tail']);
  });

  test('AgentEvent summaries are readable', () {
    AgentEvent e(Map<String, dynamic> j) => AgentEvent({'seq': 1, 'timestamp': '2026-10-14T10:00:00Z', ...j});
    expect(
      e({
        'event': 'approval_requested',
        'action': {
          'type': 'exec',
          'command': 'cargo',
          'args': ['test'],
        },
        'reason': 'tests run code',
      }).summary,
      'Needs approval: cargo test — tests run code',
    );
    expect(
      e({
        'event': 'approval_resolved',
        'approved': false,
        'by': {'kind': 'human', 'id': 'ana'},
        'comment': 'no',
      }).summary,
      'Rejected by human:ana: no',
    );
    expect(e({'event': 'status_changed', 'status': 'awaiting_input'}).summary, 'Status: awaiting input');
    expect(e({'event': 'something_new'}).summary, 'something new');
  });

  group('ApiClient', () {
    test('sends the bearer token under the server path and decodes JSON', () async {
      late http.Request seen;
      final client = ApiClient(
        server: Uri.parse('https://example.com/agentcore'),
        token: 'secret',
        httpClient: MockClient((request) async {
          seen = request;
          return http.Response(jsonEncode({'name': 'ana', 'role': 'operator'}), 200);
        }),
      );
      final me = await client.whoami();
      expect(seen.url.toString(), 'https://example.com/agentcore/api/v1/whoami');
      expect(seen.headers['Authorization'], 'Bearer secret');
      expect(me.name, 'ana');
      expect(me.role, Role.operator);
      expect(me.role.canOperate, isTrue);
    });

    test('turns error bodies into ApiException', () async {
      final client = ApiClient(
        server: Uri.parse('https://example.com'),
        httpClient: MockClient((_) async => http.Response(jsonEncode({'error': 'missing or invalid token'}), 401)),
      );
      await expectLater(
        client.sessions(),
        throwsA(isA<ApiException>().having((e) => e.unauthorized, 'unauthorized', isTrue)),
      );
    });

    test('omits optional fields and sends decisions', () async {
      final bodies = <String, Object?>{};
      final client = ApiClient(
        server: Uri.parse('https://example.com'),
        httpClient: MockClient((request) async {
          bodies[request.url.path] = request.body.isEmpty ? null : jsonDecode(request.body);
          return http.Response('', 204);
        }),
      );
      await client.decide('s1', 'a1', approved: true);
      await client.stop('s1');
      await client.pause('s1');
      expect(bodies['/api/v1/sessions/s1/approvals/a1'], {'approved': true});
      expect(bodies['/api/v1/sessions/s1/stop'], <String, Object?>{});
      expect(bodies['/api/v1/sessions/s1/pause'], isNull);
    });
  });
}
