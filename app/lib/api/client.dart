import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart' show kIsWeb;
import 'package:http/http.dart' as http;

import 'models.dart';
import 'sse.dart';

class ApiException implements Exception {
  ApiException(this.status, this.message);
  final int status;
  final String message;

  bool get unauthorized => status == 401;

  @override
  String toString() => status == 0 ? message : '$message ($status)';
}

/// Normalises what people type as a server address: adds `https://` when no
/// scheme is given and drops trailing slashes. Returns null if unusable.
Uri? parseServerUrl(String input) {
  var text = input.trim();
  if (text.isEmpty) return null;
  if (!text.contains('://')) text = 'https://$text';
  final uri = Uri.tryParse(text);
  if (uri == null || (uri.scheme != 'http' && uri.scheme != 'https') || uri.host.isEmpty) {
    return null;
  }
  var path = uri.path;
  while (path.endsWith('/')) {
    path = path.substring(0, path.length - 1);
  }
  return uri.replace(path: path, query: null, fragment: null);
}

/// Client for agentcore's operator API (`/api/v1`). The server does all the
/// work; this app only shows its state and sends people's decisions.
class ApiClient {
  ApiClient({required this.server, this.token, http.Client? httpClient, http.Client Function()? streamClient})
    : _http = httpClient ?? http.Client(),
      _streamClient = streamClient ?? http.Client.new;

  /// Base URL of the agentcore server, e.g. `https://agentcore.example.com`.
  final Uri server;

  /// Operator token; null for a loopback server without operators.
  final String? token;
  final http.Client _http;

  /// Each long-lived stream gets its own client so cancelling it closes
  /// exactly that connection.
  final http.Client Function() _streamClient;

  static const _timeout = Duration(seconds: 30);

  void close() => _http.close();

  Uri uri(String path, [Map<String, String>? query]) => server.replace(
    path: '${server.path}/api/v1$path',
    queryParameters: query == null || query.isEmpty ? null : query,
  );

  Map<String, String> _headers({bool json = false, bool sse = false}) => {
    if (token != null && token!.isNotEmpty) 'Authorization': 'Bearer $token',
    if (json) 'Content-Type': 'application/json',
    'Accept': sse ? 'text/event-stream' : 'application/json',
  };

  Future<dynamic> _request(String method, String path, {Object? body, Map<String, String>? query}) async {
    final request = http.Request(method, uri(path, query))..headers.addAll(_headers(json: body != null));
    if (body != null) request.body = jsonEncode(body);
    final http.Response response;
    try {
      response = await http.Response.fromStream(await _http.send(request).timeout(_timeout));
    } on TimeoutException {
      throw ApiException(0, 'The server did not answer in time');
    } on http.ClientException catch (e) {
      throw ApiException(0, 'Cannot reach the server: ${e.message}');
    }
    final text = utf8.decode(response.bodyBytes, allowMalformed: true);
    if (response.statusCode >= 400) {
      var message = response.reasonPhrase ?? 'Request failed';
      try {
        final decoded = jsonDecode(text);
        if (decoded is Map && decoded['error'] is String) message = decoded['error'] as String;
      } on FormatException {
        // Non-JSON error body (e.g. from a reverse proxy).
      }
      throw ApiException(response.statusCode, message);
    }
    return text.isEmpty ? null : jsonDecode(text);
  }

  Future<dynamic> get(String path, [Map<String, String>? query]) => _request('GET', path, query: query);
  Future<dynamic> post(String path, [Object? body]) => _request('POST', path, body: body);
  Future<dynamic> put(String path, [Object? body]) => _request('PUT', path, body: body);

  List<T> _list<T>(dynamic json, T Function(Json) parse) =>
      (json as List? ?? const []).whereType<Json>().map(parse).toList();

  // ---- system ----

  Future<Json> health() async => (await get('/health')) as Json;
  Future<Me> whoami() async => Me.fromJson((await get('/whoami')) as Json);
  Future<int> stopAll([String? reason]) async =>
      (((await post('/stop-all', {'reason': ?reason})) as Json?)?['stopped'] as num?)?.toInt() ?? 0;

  // ---- sessions ----

  Future<List<SessionInfo>> sessions() async => _list(await get('/sessions'), SessionInfo.fromJson);
  Future<SessionDetail> session(String id) async => SessionDetail.fromJson((await get('/sessions/$id')) as Json);
  Future<List<AgentEntry>> agents() async => _list(await get('/agents'), AgentEntry.fromJson);
  Future<SessionInfo> createSession({required String agent, required String task, String? policy}) async =>
      SessionInfo.fromJson((await post('/sessions', {'agent': agent, 'task': task, 'policy': ?policy})) as Json);
  Future<List<PendingApproval>> approvals(String id) async =>
      _list(await get('/sessions/$id/approvals'), PendingApproval.fromJson);
  Future<void> decide(String sessionId, String approvalId, {required bool approved, String? comment}) =>
      post('/sessions/$sessionId/approvals/$approvalId', {'approved': approved, 'comment': ?comment});
  Future<void> stop(String id, [String? reason]) => post('/sessions/$id/stop', {'reason': ?reason});
  Future<void> pause(String id) => post('/sessions/$id/pause');
  Future<void> resume(String id) => post('/sessions/$id/resume');
  Future<void> sendMessage(String id, String text) => post('/sessions/$id/messages', {'text': text});
  Future<void> finish(String id) => post('/sessions/$id/finish');
  Future<Changes?> changes(String id) async {
    final json = await get('/sessions/$id/changes');
    return json == null ? null : Changes.fromJson(json as Json);
  }

  Future<List<AgentEvent>> events(String id, {int? after}) async =>
      _list(await get('/sessions/$id/events', {if (after != null) 'after': '$after'}), AgentEvent.new);

  /// A session's events: history first, then live ones, reconnecting after
  /// network errors and resuming after the last seen sequence number.
  ///
  /// Browsers cannot stream a response body through `package:http`, so the
  /// web build polls `/events` instead of reading the SSE stream.
  Stream<AgentEvent> sessionEvents(String id) {
    int? lastSeq;
    late final _Loop<AgentEvent> loop;

    void emit(AgentEvent e) {
      if (lastSeq == null || e.seq > lastSeq!) {
        lastSeq = e.seq;
        loop.add(e);
      }
    }

    loop = _Loop<AgentEvent>(_streamClient, (client) async {
      try {
        if (kIsWeb) {
          for (final e in await events(id, after: lastSeq)) {
            emit(e);
          }
          return const Duration(seconds: 2);
        }
        final request = http.Request('GET', uri('/sessions/$id/stream', {if (lastSeq != null) 'after': '$lastSeq'}))
          ..headers.addAll(_headers(sse: true));
        final response = await client.send(request);
        if (response.statusCode >= 400) throw ApiException(response.statusCode, 'stream failed');
        await for (final msg in response.stream.transform(utf8.decoder).transform(const SseDecoder())) {
          if (loop.cancelled) break;
          if (msg.event == 'event') emit(AgentEvent(jsonDecode(msg.data) as Json));
        }
      } on ApiException catch (e) {
        // Unknown session or revoked token: retrying will not help.
        if (e.status == 404 || e.status == 401 || e.status == 403) return null;
      } catch (_) {
        // Network error or the server closed the stream; reconnect.
      }
      return const Duration(seconds: 2);
    });
    return loop.stream;
  }

  /// Fires whenever something in the organisation changes (or might have,
  /// after a reconnect); listeners refetch what they show. The web build
  /// polls instead.
  Stream<void> orgChanges() {
    late final _Loop<void> loop;
    loop = _Loop<void>(_streamClient, (client) async {
      if (kIsWeb) {
        loop.add(null);
        return const Duration(seconds: 10);
      }
      try {
        final response = await client.send(
          http.Request('GET', uri('/org/stream'))..headers.addAll(_headers(sse: true)),
        );
        if (response.statusCode >= 400) throw ApiException(response.statusCode, 'stream failed');
        loop.add(null);
        await for (final msg in response.stream.transform(utf8.decoder).transform(const SseDecoder())) {
          if (loop.cancelled) break;
          if (msg.event == 'org' || msg.event == 'lagged') loop.add(null);
        }
      } on ApiException catch (e) {
        if (e.status == 401 || e.status == 403) return null;
      } catch (_) {
        // Reconnect below.
      }
      return const Duration(seconds: 3);
    });
    return loop.stream;
  }

  // ---- organisation ----

  Future<OrgOverview> org() async => OrgOverview.fromJson((await get('/org')) as Json);
  Future<void> controlDepartment(String id, String control) => post('/org/departments/$id/$control');
  Future<void> controlAgent(String id, String control) => post('/org/agents/$id/$control');
  Future<void> pauseAll() => post('/org/pause-all');
  Future<void> resumeAll() => post('/org/resume-all');
  Future<List<OrgMessage>> messages({String? department, int limit = 100}) async =>
      _list(await get('/org/messages', {'department': ?department, 'limit': '$limit'}), OrgMessage.fromJson);

  /// [to] is `{'agent': id}`, `{'department': id}` or `'all_departments'`.
  Future<void> postMessage(Object to, String text) => post('/org/messages', {'to': to, 'text': text});

  // ---- outbox ----

  Future<List<OutboxItem>> outbox({String? status}) async =>
      _list(await get('/org/outbox', {'status': ?status}), OutboxItem.fromJson);
  Future<void> sendOutbox(String id, int revision) => post('/org/outbox/$id/send', {'revision': revision});
  Future<void> outboxChanges(String id, String text) => post('/org/outbox/$id/changes', {'text': text});
  Future<void> rejectOutbox(String id, String text) => post('/org/outbox/$id/reject', {'text': text});

  // ---- releases ----

  Future<List<Release>> releases({String? status}) async =>
      _list(await get('/releases', {'status': ?status}), Release.fromJson);
  Future<Map<String, String>> productNames() async {
    final list = (await get('/releases/products')) as List? ?? const [];
    return {
      for (final s in list.whereType<Json>())
        if (s['product'] case final Json p) p['id'] as String: p['name'] as String,
    };
  }

  Future<void> approveRelease(String id, int revision) => post('/releases/$id/approve', {'revision': revision});
  Future<void> releaseChanges(String id, String text) => post('/releases/$id/changes', {'text': text});
  Future<void> rejectRelease(String id, String text) => post('/releases/$id/reject', {'text': text});
}

/// Runs [step] over and over with a fresh HTTP client until the listener
/// cancels or [step] returns null; waits the returned delay in between.
/// Cancelling closes the open connection and the pending wait at once.
class _Loop<T> {
  _Loop(this._newClient, this._step) {
    _controller = StreamController<T>(onListen: _run, onCancel: _cancel);
  }

  final http.Client Function() _newClient;
  final Future<Duration?> Function(http.Client client) _step;
  late final StreamController<T> _controller;
  http.Client? _client;
  Timer? _wait;
  Completer<void>? _waiting;
  bool cancelled = false;

  Stream<T> get stream => _controller.stream;

  void add(T value) {
    if (!cancelled) _controller.add(value);
  }

  Future<void> _run() async {
    while (!cancelled) {
      final client = _client = _newClient();
      Duration? delay;
      try {
        delay = await _step(client);
      } finally {
        client.close();
        _client = null;
      }
      if (delay == null || cancelled) break;
      final waiting = _waiting = Completer<void>();
      _wait = Timer(delay, waiting.complete);
      await waiting.future;
    }
    if (!_controller.isClosed) await _controller.close();
  }

  void _cancel() {
    cancelled = true;
    _client?.close();
    _wait?.cancel();
    if (_waiting?.isCompleted == false) _waiting!.complete();
  }
}
