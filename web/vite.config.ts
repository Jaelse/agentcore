import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";

// In development the UI runs on :5173 and proxies API calls to agentcore.
// AGENTCORE_DEV_API (see scripts/dev.sh) points the proxy at another backend.
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, ".", "AGENTCORE_");
  return {
    plugins: [react()],
    server: {
      proxy: {
        "/api": {
          target: env.AGENTCORE_DEV_API ?? "http://127.0.0.1:8080",
          changeOrigin: false,
        },
      },
    },
  };
});
