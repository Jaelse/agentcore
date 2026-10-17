import 'package:flutter/material.dart';

import 'state/app_state.dart';
import 'ui/connect_screen.dart';
import 'ui/home_screen.dart';

void main() {
  final state = AppState(store: SecureCredentialStore())..restore();
  runApp(AgentcoreApp(state: state));
}

class AgentcoreApp extends StatelessWidget {
  const AgentcoreApp({super.key, required this.state});

  final AppState state;

  @override
  Widget build(BuildContext context) {
    const seed = Color(0xFF3D5AFE);
    return AppScope(
      state: state,
      child: MaterialApp(
        title: 'agentcore',
        debugShowCheckedModeBanner: false,
        theme: ThemeData(colorSchemeSeed: seed, useMaterial3: true),
        darkTheme: ThemeData(colorSchemeSeed: seed, brightness: Brightness.dark, useMaterial3: true),
        home: const _Root(),
      ),
    );
  }
}

class _Root extends StatelessWidget {
  const _Root();

  @override
  Widget build(BuildContext context) {
    final app = AppScope.of(context);
    if (app.loading) return const Scaffold(body: Center(child: CircularProgressIndicator()));
    // Keyed by server and user so every screen starts fresh after switching.
    return app.signedIn ? HomeScreen(key: ValueKey('${app.client!.server}|${app.me!.name}')) : const ConnectScreen();
  }
}
