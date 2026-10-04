import { useState } from "react";

export function Login({ hadToken, onSubmit }: { hadToken: boolean; onSubmit: (token: string) => void }) {
  const [token, setToken] = useState("");
  return (
    <div className="login">
      <form
        className="card login-card"
        onSubmit={(e) => {
          e.preventDefault();
          if (token.trim()) onSubmit(token.trim());
        }}
      >
        <h1>agentcore</h1>
        <p className="muted">Sign in with your operator token to supervise agents.</p>
        {hadToken && <p className="error-text">That token was not accepted.</p>}
        <label>
          Access token
          <input
            type="password"
            autoFocus
            autoComplete="current-password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
          />
        </label>
        <button className="btn primary" type="submit">
          Sign in
        </button>
      </form>
    </div>
  );
}
