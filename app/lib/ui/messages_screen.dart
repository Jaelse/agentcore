import 'dart:async';

import 'package:flutter/material.dart';

import '../api/client.dart';
import '../api/models.dart';
import '../state/app_state.dart';
import 'common.dart';

/// The organisation's conversation: what agents tell each other, and a box
/// to talk to a department, an agent or everyone.
class MessagesScreen extends StatefulWidget {
  const MessagesScreen({super.key});

  @override
  State<MessagesScreen> createState() => _MessagesScreenState();
}

/// Who a message goes to: `'all_departments'`, `{'department': id}` or `{'agent': id}`.
class _Target {
  const _Target(this.label, this.to);
  final String label;
  final Object to;
}

class _MessagesScreenState extends State<MessagesScreen> {
  late final ApiClient _client = AppScope.read(context).client!;
  List<OrgMessage> _messages = const [];
  List<_Target> _targets = const [];
  int _target = 0;
  Object? _error;
  StreamSubscription<void>? _changes;
  Timer? _debounce;
  final _text = TextEditingController();
  final _scroll = ScrollController();

  @override
  void initState() {
    super.initState();
    _loadTargets();
    _changes = _client.orgChanges().listen((_) {
      _debounce?.cancel();
      _debounce = Timer(const Duration(milliseconds: 400), _load);
    });
  }

  @override
  void dispose() {
    _changes?.cancel();
    _debounce?.cancel();
    _text.dispose();
    _scroll.dispose();
    super.dispose();
  }

  Future<void> _loadTargets() async {
    try {
      final org = await _client.org();
      if (!mounted) return;
      setState(() {
        _targets = [
          const _Target('All departments', 'all_departments'),
          for (final d in org.departments) ...[
            _Target(d.name, {'department': d.id}),
            for (final a in d.agents) _Target('${d.name} › ${a.name}', {'agent': a.id}),
          ],
        ];
      });
    } catch (_) {
      // Sending stays unavailable; reading still works.
    }
  }

  Future<void> _load() async {
    try {
      final messages = await _client.messages(limit: 200);
      if (!mounted) return;
      final atBottom = !_scroll.hasClients || _scroll.position.extentAfter < 80;
      setState(() {
        _messages = messages;
        _error = null;
      });
      if (atBottom) {
        WidgetsBinding.instance.addPostFrameCallback((_) {
          if (_scroll.hasClients) _scroll.jumpTo(_scroll.position.maxScrollExtent);
        });
      }
    } catch (e) {
      if (mounted) setState(() => _error = e);
    }
  }

  Future<void> _send() async {
    final text = _text.text.trim();
    if (text.isEmpty || _targets.isEmpty) return;
    if (await runAction(context, () => _client.postMessage(_targets[_target].to, text))) {
      _text.clear();
      await _load();
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final canOperate = AppScope.of(context).canOperate;
    return Scaffold(
      appBar: AppBar(title: const Text('Messages')),
      body: Column(
        children: [
          Expanded(
            child: _error != null && _messages.isEmpty
                ? Center(child: Text('$_error'))
                : _messages.isEmpty
                ? const EmptyList('No messages yet.', icon: Icons.forum_outlined)
                : ListView.builder(
                    controller: _scroll,
                    padding: const EdgeInsets.all(12),
                    itemCount: _messages.length,
                    itemBuilder: (context, i) {
                      final m = _messages[i];
                      final human = m.scope == 'human';
                      return Card(
                        color: human ? theme.colorScheme.primaryContainer : null,
                        margin: const EdgeInsets.symmetric(vertical: 4),
                        child: Padding(
                          padding: const EdgeInsets.all(10),
                          child: Column(
                            crossAxisAlignment: CrossAxisAlignment.start,
                            children: [
                              Row(
                                children: [
                                  Expanded(
                                    child: Text(
                                      '${m.fromName} → ${m.toName}',
                                      style: theme.textTheme.labelLarge,
                                      overflow: TextOverflow.ellipsis,
                                    ),
                                  ),
                                  Text(timeAgo(m.createdAt), style: theme.textTheme.bodySmall),
                                ],
                              ),
                              const SizedBox(height: 4),
                              SelectableText(m.text),
                            ],
                          ),
                        ),
                      );
                    },
                  ),
          ),
          if (canOperate && _targets.isNotEmpty)
            SafeArea(
              top: false,
              child: Container(
                padding: const EdgeInsets.fromLTRB(12, 8, 8, 8),
                decoration: BoxDecoration(
                  border: Border(top: BorderSide(color: theme.dividerColor)),
                ),
                child: Column(
                  children: [
                    DropdownButton<int>(
                      value: _target,
                      isExpanded: true,
                      isDense: true,
                      items: [
                        for (var i = 0; i < _targets.length; i++)
                          DropdownMenuItem(
                            value: i,
                            child: Text('To: ${_targets[i].label}', overflow: TextOverflow.ellipsis),
                          ),
                      ],
                      onChanged: (v) => setState(() => _target = v ?? 0),
                    ),
                    Row(
                      children: [
                        Expanded(
                          child: TextField(
                            controller: _text,
                            minLines: 1,
                            maxLines: 5,
                            decoration: const InputDecoration(hintText: 'Message…', border: InputBorder.none),
                          ),
                        ),
                        IconButton.filled(tooltip: 'Send', icon: const Icon(Icons.send), onPressed: _send),
                      ],
                    ),
                  ],
                ),
              ),
            ),
        ],
      ),
    );
  }
}
