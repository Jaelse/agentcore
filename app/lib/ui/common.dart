import 'package:flutter/material.dart';

import '../api/client.dart';
import '../api/models.dart';
import '../state/app_state.dart';

String timeAgo(DateTime? t) {
  if (t == null) return '';
  final d = DateTime.now().difference(t);
  if (d.inSeconds < 60) return 'just now';
  if (d.inMinutes < 60) return '${d.inMinutes} min ago';
  if (d.inHours < 24) return '${d.inHours} h ago';
  if (d.inDays < 7) return '${d.inDays} d ago';
  return '${t.year}-${_two(t.month)}-${_two(t.day)}';
}

String clock(DateTime? t) => t == null ? '' : '${_two(t.hour)}:${_two(t.minute)}:${_two(t.second)}';

String _two(int n) => n.toString().padLeft(2, '0');

Color statusColor(BuildContext context, SessionStatus s) {
  final scheme = Theme.of(context).colorScheme;
  return switch (s) {
    SessionStatus.running || SessionStatus.pending => Colors.blue,
    SessionStatus.awaitingApproval => Colors.orange,
    SessionStatus.awaitingInput => Colors.teal,
    SessionStatus.paused => Colors.amber.shade700,
    SessionStatus.completed => Colors.green,
    SessionStatus.failed || SessionStatus.stopped => scheme.error,
    SessionStatus.unknown => scheme.outline,
  };
}

class StatusChip extends StatelessWidget {
  const StatusChip(this.label, {super.key, required this.color});

  StatusChip.session(SessionStatus status, {super.key, required BuildContext context})
    : label = status.label,
      color = statusColor(context, status);

  final String label;
  final Color color;

  @override
  Widget build(BuildContext context) => Container(
    padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
    decoration: BoxDecoration(
      color: color.withValues(alpha: 0.12),
      borderRadius: BorderRadius.circular(12),
      border: Border.all(color: color.withValues(alpha: 0.5)),
    ),
    child: Text(
      label,
      style: TextStyle(color: color, fontSize: 12, fontWeight: FontWeight.w600),
    ),
  );
}

/// Runs a server call for a button, reporting failures in a snack bar.
/// A 401 means the token was revoked: the app signs out.
Future<bool> runAction(BuildContext context, Future<void> Function() action, {String? done}) async {
  final messenger = ScaffoldMessenger.of(context);
  final app = AppScope.read(context);
  try {
    await action();
    if (done != null) messenger.showSnackBar(SnackBar(content: Text(done)));
    return true;
  } on ApiException catch (e) {
    if (e.unauthorized) {
      await app.signOut(reason: 'Your token is no longer accepted. Sign in again.');
    } else {
      messenger.showSnackBar(SnackBar(content: Text(e.toString())));
    }
  } catch (e) {
    messenger.showSnackBar(SnackBar(content: Text('$e')));
  }
  return false;
}

Future<bool> confirm(
  BuildContext context, {
  required String title,
  required String message,
  required String action,
  bool destructive = false,
}) async {
  final result = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text(title),
      content: Text(message),
      actions: [
        TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
        FilledButton(
          style: destructive ? FilledButton.styleFrom(backgroundColor: Theme.of(context).colorScheme.error) : null,
          onPressed: () => Navigator.pop(context, true),
          child: Text(action),
        ),
      ],
    ),
  );
  return result ?? false;
}

/// Asks for a line of text (a comment, a reason). Returns null on cancel.
Future<String?> promptText(
  BuildContext context, {
  required String title,
  required String action,
  String hint = '',
  bool required = false,
  bool destructive = false,
}) {
  return showDialog<String>(
    context: context,
    builder: (context) =>
        _PromptDialog(title: title, action: action, hint: hint, required: required, destructive: destructive),
  );
}

class _PromptDialog extends StatefulWidget {
  const _PromptDialog({
    required this.title,
    required this.action,
    required this.hint,
    required this.required,
    required this.destructive,
  });

  final String title;
  final String action;
  final String hint;
  final bool required;
  final bool destructive;

  @override
  State<_PromptDialog> createState() => _PromptDialogState();
}

class _PromptDialogState extends State<_PromptDialog> {
  final _text = TextEditingController();

  @override
  void dispose() {
    _text.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: Text(widget.title),
      content: TextField(
        controller: _text,
        autofocus: true,
        minLines: 1,
        maxLines: 5,
        decoration: InputDecoration(hintText: widget.hint),
        onChanged: (_) => setState(() {}),
      ),
      actions: [
        TextButton(onPressed: () => Navigator.pop(context), child: const Text('Cancel')),
        FilledButton(
          style: widget.destructive
              ? FilledButton.styleFrom(backgroundColor: Theme.of(context).colorScheme.error)
              : null,
          onPressed: widget.required && _text.text.trim().isEmpty
              ? null
              : () => Navigator.pop(context, _text.text.trim()),
          child: Text(widget.action),
        ),
      ],
    );
  }
}

/// Loads data with [load], shows progress and errors, and supports
/// pull-to-refresh. Call [AsyncListState.reload] to refetch.
class AsyncList<T> extends StatefulWidget {
  const AsyncList({super.key, required this.load, required this.builder, this.empty = 'Nothing here.'});

  final Future<T> Function() load;
  final Widget Function(BuildContext context, T data) builder;
  final String empty;

  @override
  State<AsyncList<T>> createState() => AsyncListState<T>();
}

class AsyncListState<T> extends State<AsyncList<T>> {
  T? _data;
  Object? _error;
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    reload();
  }

  Future<void> reload() async {
    try {
      final data = await widget.load();
      if (!mounted) return;
      setState(() {
        _data = data;
        _error = null;
        _loading = false;
      });
    } catch (e) {
      if (!mounted) return;
      if (e is ApiException && e.unauthorized) {
        await AppScope.read(context).signOut(reason: 'Your token is no longer accepted. Sign in again.');
        return;
      }
      setState(() {
        _error = e;
        _loading = false;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    if (_loading && _data == null) return const Center(child: CircularProgressIndicator());
    final Widget body;
    if (_data == null) {
      body = ListView(
        physics: const AlwaysScrollableScrollPhysics(),
        children: [
          Padding(
            padding: const EdgeInsets.all(24),
            child: Column(
              children: [
                Icon(Icons.cloud_off, size: 48, color: Theme.of(context).colorScheme.error),
                const SizedBox(height: 12),
                Text('$_error', textAlign: TextAlign.center),
                const SizedBox(height: 12),
                OutlinedButton(onPressed: reload, child: const Text('Try again')),
              ],
            ),
          ),
        ],
      );
    } else {
      body = widget.builder(context, _data as T);
    }
    return RefreshIndicator(onRefresh: reload, child: body);
  }
}

/// A scrollable placeholder for empty lists (keeps pull-to-refresh working).
class EmptyList extends StatelessWidget {
  const EmptyList(this.message, {super.key, this.icon = Icons.inbox_outlined});
  final String message;
  final IconData icon;

  @override
  Widget build(BuildContext context) => ListView(
    physics: const AlwaysScrollableScrollPhysics(),
    children: [
      const SizedBox(height: 80),
      Icon(icon, size: 48, color: Theme.of(context).colorScheme.outline),
      const SizedBox(height: 12),
      Text(
        message,
        textAlign: TextAlign.center,
        style: TextStyle(color: Theme.of(context).colorScheme.outline),
      ),
    ],
  );
}
