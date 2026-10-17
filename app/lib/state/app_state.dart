import 'package:flutter/widgets.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';

import '../api/client.dart';
import '../api/models.dart';

/// Where the server address and operator token are kept between launches.
abstract class CredentialStore {
  Future<({String server, String? token})?> load();
  Future<void> save(String server, String? token);
  Future<void> clear();
}

/// Keychain / Keystore / libsecret; browser storage on the web.
class SecureCredentialStore implements CredentialStore {
  static const _storage = FlutterSecureStorage();
  static const _serverKey = 'agentcore.server';
  static const _tokenKey = 'agentcore.token';

  @override
  Future<({String server, String? token})?> load() async {
    final server = await _storage.read(key: _serverKey);
    if (server == null) return null;
    return (server: server, token: await _storage.read(key: _tokenKey));
  }

  @override
  Future<void> save(String server, String? token) async {
    await _storage.write(key: _serverKey, value: server);
    if (token == null || token.isEmpty) {
      await _storage.delete(key: _tokenKey);
    } else {
      await _storage.write(key: _tokenKey, value: token);
    }
  }

  @override
  Future<void> clear() async {
    await _storage.delete(key: _serverKey);
    await _storage.delete(key: _tokenKey);
  }
}

class MemoryCredentialStore implements CredentialStore {
  ({String server, String? token})? value;

  @override
  Future<({String server, String? token})?> load() async => value;
  @override
  Future<void> save(String server, String? token) async => value = (server: server, token: token);
  @override
  Future<void> clear() async => value = null;
}

typedef ClientFactory = ApiClient Function(Uri server, String? token);

/// The connection to one agentcore server and who is signed in.
class AppState extends ChangeNotifier {
  AppState({required this.store, ClientFactory? clientFactory})
    : _clientFactory = clientFactory ?? ((server, token) => ApiClient(server: server, token: token));

  final CredentialStore store;
  final ClientFactory _clientFactory;

  bool loading = true;
  ApiClient? client;
  Me? me;

  /// The last server address used, to prefill the sign-in form.
  String? lastServer;

  /// Why a saved sign-in no longer works, shown on the sign-in screen.
  String? notice;

  bool get signedIn => client != null && me != null;
  bool get canOperate => me?.role.canOperate ?? false;

  Future<void> restore() async {
    try {
      final saved = await store.load();
      if (saved != null) {
        lastServer = saved.server;
        final server = parseServerUrl(saved.server);
        if (server != null) await _connect(server, saved.token);
      }
    } on ApiException catch (e) {
      notice = e.unauthorized ? 'Your token is no longer accepted. Sign in again.' : e.toString();
    } catch (e) {
      notice = '$e';
    }
    loading = false;
    notifyListeners();
  }

  /// Checks the server and token, then remembers them. Throws [ApiException].
  Future<void> signIn(String serverInput, String token) async {
    final server = parseServerUrl(serverInput);
    if (server == null) throw ApiException(0, 'Enter the server address, e.g. https://agentcore.example.com');
    final trimmed = token.trim();
    await _connect(server, trimmed.isEmpty ? null : trimmed);
    await store.save(server.toString(), trimmed.isEmpty ? null : trimmed);
    lastServer = server.toString();
    notice = null;
    notifyListeners();
  }

  Future<void> _connect(Uri server, String? token) async {
    final candidate = _clientFactory(server, token);
    try {
      await candidate.health();
      final who = await candidate.whoami();
      client?.close();
      client = candidate;
      me = who;
    } catch (_) {
      candidate.close();
      rethrow;
    }
  }

  Future<void> signOut({String? reason}) async {
    await store.clear();
    client?.close();
    client = null;
    me = null;
    notice = reason;
    notifyListeners();
  }
}

/// Makes [AppState] available to every screen.
class AppScope extends InheritedNotifier<AppState> {
  const AppScope({super.key, required AppState state, required super.child}) : super(notifier: state);

  static AppState of(BuildContext context) => context.dependOnInheritedWidgetOfExactType<AppScope>()!.notifier!;

  /// Read without rebuilding when the state changes (for callbacks).
  static AppState read(BuildContext context) =>
      (context.getElementForInheritedWidgetOfExactType<AppScope>()!.widget as AppScope).notifier!;
}
