import 'dart:convert';

import 'package:agentcore_app/api/client.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

/// A tiny in-memory agentcore server for widget tests.
class FakeServer {
  FakeServer({this.token = 'good', this.role = 'operator'});

  final String token;
  final String role;
  final requests = <String>[];

  final sessions = <Map<String, dynamic>>[
    {
      'id': 's1',
      'agent': 'opencode',
      'task': 'Fix the login bug',
      'policy': 'default',
      'status': 'awaiting_approval',
      'created_by': {'kind': 'human', 'id': 'ana'},
      'created_at': '2026-10-14T10:00:00Z',
      'ended_at': null,
      'pending_approvals': 1,
      'actions': 3,
      'model_calls': 2,
      'context': {
        'project_name': 'web',
        'issue': {'number': 42, 'title': 'Login', 'url': ''},
      },
    },
    {
      'id': 's0',
      'agent': 'demo',
      'task': 'Old task',
      'policy': 'default',
      'status': 'completed',
      'created_by': {'kind': 'human', 'id': 'ana'},
      'created_at': '2026-10-13T10:00:00Z',
      'ended_at': '2026-10-13T11:00:00Z',
      'pending_approvals': 0,
      'actions': 1,
      'model_calls': 1,
      'context': <String, dynamic>{},
    },
  ];

  final approvals = <Map<String, dynamic>>[
    {
      'approval_id': 'a1',
      'action_id': 'x1',
      'action': {
        'type': 'exec',
        'command': 'npm',
        'args': ['publish'],
      },
      'reason': 'publishing needs a person',
      'requested_at': '2026-10-14T10:01:00Z',
      'expires_at': '2026-10-14T10:11:00Z',
    },
  ];

  ApiClient client(Uri server, String? token) =>
      ApiClient(server: server, token: token, httpClient: MockClient((request) async => _handle(request)));

  http.Response _json(Object? body, [int status = 200]) =>
      http.Response(jsonEncode(body), status, headers: {'content-type': 'application/json'});

  http.Response _handle(http.Request request) {
    final path = request.url.path.replaceFirst('/api/v1', '');
    requests.add('${request.method} $path');
    if (path == '/health') return _json({'status': 'ok', 'version': 'test'});
    if (request.headers['Authorization'] != 'Bearer $token') return _json({'error': 'missing or invalid token'}, 401);
    switch ((request.method, path)) {
      case ('GET', '/whoami'):
        return _json({'name': 'ana', 'role': role});
      case ('GET', '/sessions'):
        return _json(sessions);
      case ('GET', '/sessions/s1'):
        return _json({
          'session': sessions[0],
          'live': true,
          'policy': {'name': 'default', 'digest': 'd'},
          'approvals': approvals,
          'proposal': null,
          'role': null,
        });
      case ('GET', '/sessions/s1/approvals'):
        return _json(approvals);
      case ('GET', '/sessions/s1/events'):
      case ('GET', '/sessions/s1/stream'):
      case ('GET', '/org/stream'):
        return http.Response('', 200, headers: {'content-type': 'text/event-stream'});
      case ('POST', '/sessions/s1/approvals/a1'):
        approvals.clear();
        sessions[0]['pending_approvals'] = 0;
        sessions[0]['status'] = 'running';
        return http.Response('', 204);
      case ('GET', '/org/outbox'):
      case ('GET', '/releases'):
      case ('GET', '/releases/products'):
        return _json([]);
    }
    return _json({'error': 'not found'}, 404);
  }
}
