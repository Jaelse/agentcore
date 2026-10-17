import 'package:flutter/material.dart';

import '../state/app_state.dart';
import 'common.dart';
import 'inbox_screen.dart';
import 'org_screen.dart';
import 'sessions_screen.dart';

class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key});

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  int _tab = 0;

  static const _titles = ['Sessions', 'Inbox', 'Organisation'];

  Future<void> _stopAll() async {
    final reason = await promptText(
      context,
      title: 'Stop every agent?',
      action: 'STOP ALL',
      hint: 'Reason (recorded in the audit log)',
      destructive: true,
    );
    if (reason == null || !mounted) return;
    final client = AppScope.read(context).client!;
    var stopped = 0;
    final ok = await runAction(context, () async {
      stopped = await client.stopAll(reason.isEmpty ? null : reason);
    });
    if (ok && mounted) {
      ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text('Stopped $stopped session(s).')));
    }
  }

  @override
  Widget build(BuildContext context) {
    final app = AppScope.of(context);
    final me = app.me!;
    final scheme = Theme.of(context).colorScheme;
    return Scaffold(
      appBar: AppBar(
        title: Text(_titles[_tab]),
        actions: [
          if (app.canOperate)
            Padding(
              padding: const EdgeInsets.only(right: 4),
              child: TextButton.icon(
                style: TextButton.styleFrom(foregroundColor: scheme.error),
                onPressed: _stopAll,
                icon: const Icon(Icons.stop_circle_outlined),
                label: const Text('Stop all'),
              ),
            ),
          PopupMenuButton<String>(
            tooltip: 'Account',
            icon: const Icon(Icons.account_circle_outlined),
            onSelected: (v) {
              if (v == 'signout') app.signOut();
            },
            itemBuilder: (context) => [
              PopupMenuItem(
                enabled: false,
                child: ListTile(
                  contentPadding: EdgeInsets.zero,
                  title: Text('${me.name} (${me.role.name})'),
                  subtitle: Text(app.client!.server.toString()),
                ),
              ),
              const PopupMenuDivider(),
              const PopupMenuItem(value: 'signout', child: Text('Sign out')),
            ],
          ),
        ],
      ),
      body: IndexedStack(index: _tab, children: const [SessionsScreen(), InboxScreen(), OrgScreen()]),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _tab,
        onDestinationSelected: (i) => setState(() => _tab = i),
        destinations: const [
          NavigationDestination(icon: Icon(Icons.terminal), label: 'Sessions'),
          NavigationDestination(icon: Icon(Icons.inbox_outlined), selectedIcon: Icon(Icons.inbox), label: 'Inbox'),
          NavigationDestination(
            icon: Icon(Icons.apartment_outlined),
            selectedIcon: Icon(Icons.apartment),
            label: 'Organisation',
          ),
        ],
      ),
    );
  }
}
