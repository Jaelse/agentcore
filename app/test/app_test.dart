import 'package:agentcore_app/main.dart';
import 'package:agentcore_app/state/app_state.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fake_server.dart';

void main() {
  Future<(AppState, FakeServer)> start(WidgetTester tester, {String? savedToken, String role = 'operator'}) async {
    final server = FakeServer(role: role);
    final store = MemoryCredentialStore();
    if (savedToken != null) await store.save('https://agentcore.test', savedToken);
    final state = AppState(store: store, clientFactory: server.client);
    await state.restore();
    await tester.pumpWidget(AgentcoreApp(state: state));
    await tester.pump();
    return (state, server);
  }

  testWidgets('signs in, lists active sessions and remembers the server', (tester) async {
    final (state, _) = await start(tester);
    expect(find.text('Sign in'), findsOneWidget);

    await tester.enterText(find.widgetWithText(TextField, 'Server'), 'agentcore.test');
    await tester.enterText(find.widgetWithText(TextField, 'Operator token'), 'wrong');
    await tester.tap(find.text('Sign in'));
    await tester.pumpAndSettle();
    expect(find.text('That token was not accepted.'), findsOneWidget);

    await tester.enterText(find.widgetWithText(TextField, 'Operator token'), 'good');
    await tester.tap(find.text('Sign in'));
    await tester.pump();
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));

    expect(state.signedIn, isTrue);
    expect((state.store as MemoryCredentialStore).value?.server, 'https://agentcore.test');
    expect(find.text('Fix the login bug'), findsOneWidget);
    expect(find.text('Old task'), findsNothing, reason: 'finished sessions are hidden under Active');
    expect(find.text('1 to approve'), findsOneWidget);
    expect(find.text('Stop all'), findsOneWidget);

    await tester.tap(find.text('All'));
    await tester.pump();
    expect(find.text('Old task'), findsOneWidget);
    await state.signOut();
    await tester.pumpAndSettle();
  });

  testWidgets('approves a pending action from the inbox', (tester) async {
    final (state, server) = await start(tester, savedToken: 'good');
    expect(state.signedIn, isTrue);

    await tester.tap(find.text('Inbox'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(find.text('npm publish'), findsOneWidget);
    expect(find.text('publishing needs a person'), findsOneWidget);

    await tester.tap(find.widgetWithText(FilledButton, 'Approve'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(server.requests, contains('POST /sessions/s1/approvals/a1'));
    expect(find.text('Nothing is waiting for you.'), findsOneWidget);
    await state.signOut();
    await tester.pumpAndSettle();
  });

  testWidgets('viewers see no controls', (tester) async {
    final (state, _) = await start(tester, savedToken: 'good', role: 'viewer');
    await tester.pump(const Duration(milliseconds: 100));
    expect(find.text('Fix the login bug'), findsOneWidget);
    expect(find.text('Stop all'), findsNothing);
    expect(find.text('New session'), findsNothing);
    await state.signOut();
    await tester.pumpAndSettle();
  });

  testWidgets('a revoked saved token returns to sign-in with a notice', (tester) async {
    await start(tester, savedToken: 'revoked');
    expect(find.text('Sign in'), findsOneWidget);
    expect(find.text('Your token is no longer accepted. Sign in again.'), findsOneWidget);
  });

  testWidgets('a session shows its pending approval and approves it', (tester) async {
    final (state, server) = await start(tester, savedToken: 'good');
    await tester.pump(const Duration(milliseconds: 100));
    await tester.tap(find.text('Fix the login bug'));
    await tester.pumpAndSettle(const Duration(milliseconds: 100));

    expect(find.text('Needs your approval'), findsOneWidget);
    expect(find.text('npm publish'), findsOneWidget);
    await tester.tap(find.widgetWithText(FilledButton, 'Approve'));
    await tester.pumpAndSettle(const Duration(milliseconds: 100));
    expect(server.requests, contains('POST /sessions/s1/approvals/a1'));
    expect(find.text('Needs your approval'), findsNothing);
    expect(find.byTooltip('Pause'), findsOneWidget);
    expect(find.byTooltip('Stop agent'), findsOneWidget);
    await state.signOut();
    await tester.pumpAndSettle();
  });
}
