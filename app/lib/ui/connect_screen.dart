import 'package:flutter/material.dart';

import '../api/client.dart';
import '../state/app_state.dart';

/// Sign in to an agentcore server with its address and an operator token.
class ConnectScreen extends StatefulWidget {
  const ConnectScreen({super.key});

  @override
  State<ConnectScreen> createState() => _ConnectScreenState();
}

class _ConnectScreenState extends State<ConnectScreen> {
  late final TextEditingController _server;
  final _token = TextEditingController();
  bool _busy = false;
  bool _showToken = false;
  String? _error;

  @override
  void initState() {
    super.initState();
    _server = TextEditingController(text: AppScope.read(context).lastServer ?? '');
  }

  @override
  void dispose() {
    _server.dispose();
    _token.dispose();
    super.dispose();
  }

  Future<void> _submit() async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      await AppScope.read(context).signIn(_server.text, _token.text);
    } on ApiException catch (e) {
      setState(() => _error = e.unauthorized ? 'That token was not accepted.' : e.toString());
    } catch (e) {
      setState(() => _error = '$e');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final app = AppScope.of(context);
    final theme = Theme.of(context);
    final url = parseServerUrl(_server.text);
    final insecure = url != null && url.scheme == 'http' && !_isLocal(url.host);
    return Scaffold(
      body: SafeArea(
        child: Center(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(24),
            child: ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 420),
              child: AutofillGroup(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    Icon(Icons.shield_outlined, size: 56, color: theme.colorScheme.primary),
                    const SizedBox(height: 12),
                    Text('agentcore', textAlign: TextAlign.center, style: theme.textTheme.headlineMedium),
                    const SizedBox(height: 8),
                    Text(
                      'Supervise your agents from anywhere. The agents run on your '
                      'agentcore server; this app watches them, approves their actions and stops them.',
                      textAlign: TextAlign.center,
                      style: theme.textTheme.bodyMedium?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                    ),
                    const SizedBox(height: 24),
                    if (app.notice != null && _error == null) ...[
                      Text(app.notice!, style: TextStyle(color: theme.colorScheme.error)),
                      const SizedBox(height: 12),
                    ],
                    TextField(
                      controller: _server,
                      keyboardType: TextInputType.url,
                      autocorrect: false,
                      autofillHints: const [AutofillHints.url],
                      decoration: const InputDecoration(
                        labelText: 'Server',
                        hintText: 'https://agentcore.example.com',
                        border: OutlineInputBorder(),
                        prefixIcon: Icon(Icons.dns_outlined),
                      ),
                      onChanged: (_) => setState(() {}),
                    ),
                    if (insecure)
                      Padding(
                        padding: const EdgeInsets.only(top: 6),
                        child: Text(
                          'Without https your token travels unencrypted. Put a TLS reverse proxy in front of the server.',
                          style: TextStyle(color: theme.colorScheme.error, fontSize: 12),
                        ),
                      ),
                    const SizedBox(height: 16),
                    TextField(
                      controller: _token,
                      obscureText: !_showToken,
                      autocorrect: false,
                      enableSuggestions: false,
                      autofillHints: const [AutofillHints.password],
                      decoration: InputDecoration(
                        labelText: 'Operator token',
                        helperText: 'Leave empty for a local server without operators.',
                        border: const OutlineInputBorder(),
                        prefixIcon: const Icon(Icons.key_outlined),
                        suffixIcon: IconButton(
                          tooltip: _showToken ? 'Hide token' : 'Show token',
                          icon: Icon(_showToken ? Icons.visibility_off : Icons.visibility),
                          onPressed: () => setState(() => _showToken = !_showToken),
                        ),
                      ),
                      onSubmitted: (_) => _busy ? null : _submit(),
                    ),
                    if (_error != null) ...[
                      const SizedBox(height: 12),
                      Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
                    ],
                    const SizedBox(height: 24),
                    FilledButton(
                      onPressed: _busy || url == null ? null : _submit,
                      child: _busy
                          ? const SizedBox(width: 18, height: 18, child: CircularProgressIndicator(strokeWidth: 2))
                          : const Text('Sign in'),
                    ),
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }

  static bool _isLocal(String host) =>
      host == 'localhost' || host == '127.0.0.1' || host == '::1' || host == '10.0.2.2';
}
