import 'dart:async';

import 'package:flutter/material.dart';

import '../api/client.dart';
import '../api/models.dart';
import '../state/app_state.dart';
import 'common.dart';
import 'messages_screen.dart';
import 'session_screen.dart';

/// Departments and their agents, updated live from the organisation stream.
class OrgScreen extends StatefulWidget {
  const OrgScreen({super.key});

  @override
  State<OrgScreen> createState() => _OrgScreenState();
}

class _OrgScreenState extends State<OrgScreen> {
  final _list = GlobalKey<AsyncListState<OrgOverview>>();
  StreamSubscription<void>? _changes;
  Timer? _debounce;

  @override
  void initState() {
    super.initState();
    _changes = AppScope.read(context).client!.orgChanges().listen((_) {
      _debounce?.cancel();
      _debounce = Timer(const Duration(milliseconds: 500), () => _list.currentState?.reload());
    });
  }

  @override
  void dispose() {
    _changes?.cancel();
    _debounce?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final app = AppScope.of(context);
    final client = app.client!;
    return Scaffold(
      floatingActionButton: FloatingActionButton.extended(
        heroTag: 'messages',
        onPressed: () => Navigator.push(context, MaterialPageRoute<void>(builder: (_) => const MessagesScreen())),
        icon: const Icon(Icons.forum_outlined),
        label: const Text('Messages'),
      ),
      body: AsyncList<OrgOverview>(
        key: _list,
        load: client.org,
        builder: (context, org) {
          if (org.departments.isEmpty) {
            return const EmptyList('No departments yet. Build your organisation in the web UI.', icon: Icons.apartment);
          }
          return ListView(
            padding: const EdgeInsets.only(bottom: 88),
            children: [
              if (app.canOperate)
                Padding(
                  padding: const EdgeInsets.fromLTRB(12, 8, 12, 0),
                  child: Row(
                    children: [
                      if (org.companyName.isNotEmpty)
                        Expanded(child: Text(org.companyName, style: Theme.of(context).textTheme.titleMedium))
                      else
                        const Spacer(),
                      TextButton.icon(
                        onPressed: () => _confirmAll(context, client, pause: true),
                        icon: const Icon(Icons.pause_circle_outline),
                        label: const Text('Pause all'),
                      ),
                      TextButton.icon(
                        onPressed: () => _confirmAll(context, client, pause: false),
                        icon: const Icon(Icons.play_circle_outline),
                        label: const Text('Resume all'),
                      ),
                    ],
                  ),
                ),
              for (final d in org.departments)
                _DepartmentCard(
                  department: d,
                  client: client,
                  canOperate: app.canOperate,
                  onChanged: () => _list.currentState?.reload(),
                ),
            ],
          );
        },
      ),
    );
  }

  Future<void> _confirmAll(BuildContext context, ApiClient client, {required bool pause}) async {
    final ok = await confirm(
      context,
      title: pause ? 'Pause the whole organisation?' : 'Resume the whole organisation?',
      message: pause
          ? 'Every department agent freezes where it is until resumed.'
          : 'Departments continue where they were paused.',
      action: pause ? 'Pause all' : 'Resume all',
    );
    if (!ok || !context.mounted) return;
    if (await runAction(context, pause ? client.pauseAll : client.resumeAll, done: pause ? 'Paused' : 'Resumed')) {
      _list.currentState?.reload();
    }
  }
}

class _DepartmentCard extends StatelessWidget {
  const _DepartmentCard({
    required this.department,
    required this.client,
    required this.canOperate,
    required this.onChanged,
  });

  final Department department;
  final ApiClient client;
  final bool canOperate;
  final VoidCallback onChanged;

  Future<void> _control(BuildContext context, Future<void> Function() call, String done) async {
    if (await runAction(context, call, done: done)) onChanged();
  }

  @override
  Widget build(BuildContext context) {
    final d = department;
    final theme = Theme.of(context);
    final running = d.agents.where((a) => a.desired == 'running').length;
    return Card(
      margin: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      child: ExpansionTile(
        shape: const Border(),
        title: Text(d.name),
        subtitle: Padding(
          padding: const EdgeInsets.only(top: 4),
          child: Wrap(
            spacing: 8,
            children: [
              StatusChip(d.state, color: d.state == 'paused' ? Colors.amber.shade700 : Colors.green),
              Text('$running/${d.agents.length} agents on', style: theme.textTheme.bodySmall),
            ],
          ),
        ),
        trailing: canOperate
            ? _ControlMenu(
                desired: null,
                onSelected: (c) => _control(context, () => client.controlDepartment(d.id, c), '${d.name}: $c'),
              )
            : null,
        childrenPadding: const EdgeInsets.only(bottom: 8),
        children: [
          if (d.mission.isNotEmpty)
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
              child: Text(d.mission, style: theme.textTheme.bodySmall),
            ),
          for (final a in d.agents)
            ListTile(
              dense: true,
              leading: Icon(a.kind == 'communicator' ? Icons.campaign_outlined : Icons.smart_toy_outlined),
              title: Text(a.name),
              subtitle: Text(
                [a.agent, a.status != null ? SessionStatus.parse(a.status).label : a.desired, ?a.note].join(' · '),
              ),
              onTap: a.sessionId == null
                  ? null
                  : () => Navigator.push(
                      context,
                      MaterialPageRoute<void>(builder: (_) => SessionScreen(sessionId: a.sessionId!)),
                    ),
              trailing: canOperate
                  ? _ControlMenu(
                      desired: a.desired,
                      onSelected: (c) => _control(context, () => client.controlAgent(a.id, c), '${a.name}: $c'),
                    )
                  : null,
            ),
        ],
      ),
    );
  }
}

class _ControlMenu extends StatelessWidget {
  const _ControlMenu({required this.desired, required this.onSelected});

  /// `running`, `paused` or `stopped`; null offers every control (a
  /// department's agents can be in different states).
  final String? desired;
  final void Function(String control) onSelected;

  @override
  Widget build(BuildContext context) {
    final error = Theme.of(context).colorScheme.error;
    return PopupMenuButton<String>(
      tooltip: 'Control',
      onSelected: (c) async {
        if (c == 'stop' &&
            !await confirm(
              context,
              title: 'Stop?',
              message: 'Running agents are stopped now.',
              action: 'Stop',
              destructive: true,
            )) {
          return;
        }
        onSelected(c);
      },
      itemBuilder: (context) => [
        if (desired == null || desired == 'stopped')
          const PopupMenuItem(
            value: 'start',
            child: ListTile(leading: Icon(Icons.play_arrow), title: Text('Start')),
          ),
        if (desired == null || desired == 'running')
          const PopupMenuItem(
            value: 'pause',
            child: ListTile(leading: Icon(Icons.pause), title: Text('Pause')),
          ),
        if (desired == null || desired == 'paused')
          const PopupMenuItem(
            value: 'resume',
            child: ListTile(leading: Icon(Icons.play_arrow), title: Text('Resume')),
          ),
        if (desired != 'stopped')
          PopupMenuItem(
            value: 'stop',
            child: ListTile(
              leading: Icon(Icons.stop, color: error),
              title: Text('Stop', style: TextStyle(color: error)),
            ),
          ),
      ],
    );
  }
}
