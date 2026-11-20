import * as Alchemy from "alchemy";
import * as Cloudflare from "@alchemy.run/cloudflare";
import * as Config from "effect/Config";
import * as Effect from "effect/Effect";

// Servers, profiles and settings are edited in the relay's web app and stored
// in KV. Only two secrets come from the deploy environment (.env):
//   RELAY_AUTH_TOKEN      bearer token for MCP clients and the web app
//   RELAY_ENCRYPTION_KEY  encrypts upstream tokens at rest (never change it)
export default Alchemy.Stack(
  "EndaraRelay",
  {
    providers: Cloudflare.providers(),
    state: Cloudflare.state(),
  },
  Effect.gen(function* () {
    const cache = yield* Cloudflare.KV.Namespace("RelayCache");

    const relay = yield* Cloudflare.Worker("Relay", {
      // Output of `worker-build --release` (run by `npm run deploy`).
      main: "./build/index.js",
      bundle: false,
      compatibility: { date: "2026-10-01", flags: [] },
      env: {
        RELAY_CACHE: cache,
        RELAY_AUTH_TOKEN: Config.Redacted("RELAY_AUTH_TOKEN"),
        RELAY_ENCRYPTION_KEY: Config.Redacted("RELAY_ENCRYPTION_KEY"),
      },
    });

    return { url: relay.url };
  }),
);
