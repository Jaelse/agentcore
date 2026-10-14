import 'package:flutter/material.dart';

import '../api/models.dart';
import '../state/app_state.dart';
import 'common.dart';

/// Start an agent on a free task (no project). The server picks the sandbox,
/// policy and model; this only says which agent and what to do.
class NewSessionScreen extends StatefulWidget {
  const NewSessionScreen({super.key});

  @override
  State<NewSessionScreen> createState() => _NewSessionScreenState();
}

class _NewSessionScreenState extends State<NewSessionScreen> {
  final _task = TextEditingController();
  final _policy = TextEditingController();
  late final Future<List<AgentEntry>> _agents = AppScope.read(context).client!.agents();
  String? _agent;
  bool _busy = false;

  @override
  void dispose() {
    _task.dispose();
    _policy.dispose();
    super.dispose();
  }

  Future<void> _start() async {
    setState(() => _busy = true);
    final client = AppScope.read(context).client!;
    SessionInfo? created;
    await runAction(context, () async {
      created = await client.createSession(
        agent: _agent!,
        task: _task.text.trim(),
        policy: _policy.text.trim().isEmpty ? null : _policy.text.trim(),
      );
    });
    if (!mounted) return;
    setState(() => _busy = false);
    if (created != null) Navigator.pop(context, created);
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('New session')),
      body: FutureBuilder<List<AgentEntry>>(
        future: _agents,
        builder: (context, snap) {
          if (snap.hasError) return Center(child: Text('${snap.error}'));
          if (!snap.hasData) return const Center(child: CircularProgressIndicator());
          final agents = snap.data!.where((a) => a.enabled).toList();
          if (agents.isEmpty) {
            return const Center(child: Text('No agents are configured on this server.'));
          }
          _agent ??= agents.first.name;
          return ListView(
            padding: const EdgeInsets.all(16),
            children: [
              DropdownButtonFormField<String>(
                initialValue: _agent,
                decoration: const InputDecoration(labelText: 'Agent', border: OutlineInputBorder()),
                items: [
                  for (final a in agents)
                    DropdownMenuItem(
                      value: a.name,
                      child: Text(
                        a.description.isEmpty ? a.name : '${a.name} — ${a.description}',
                        overflow: TextOverflow.ellipsis,
                      ),
                    ),
                ],
                isExpanded: true,
                onChanged: (v) => setState(() => _agent = v),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: _task,
                minLines: 4,
                maxLines: 12,
                decoration: const InputDecoration(
                  labelText: 'Task',
                  hintText: 'What should the agent do?',
                  border: OutlineInputBorder(),
                  alignLabelWithHint: true,
                ),
                onChanged: (_) => setState(() {}),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: _policy,
                decoration: const InputDecoration(
                  labelText: 'Policy (optional)',
                  helperText: "Defaults to the agent's policy.",
                  border: OutlineInputBorder(),
                ),
              ),
              const SizedBox(height: 24),
              FilledButton.icon(
                onPressed: _busy || _agent == null || _task.text.trim().isEmpty ? null : _start,
                icon: const Icon(Icons.play_arrow),
                label: const Text('Start agent'),
              ),
            ],
          );
        },
      ),
    );
  }
}
