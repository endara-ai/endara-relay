import * as Alchemy from "alchemy";
import * as Cloudflare from "@alchemy.run/cloudflare";
import * as Config from "effect/Config";
import * as Effect from "effect/Effect";
import * as Redacted from "effect/Redacted";
import { readFileSync } from "node:fs";

// The relay config is the local relay's TOML. `$VAR` / `${VAR}` references in
// endpoint URLs and headers are bound as Worker secrets read from the deploy
// environment (or `--env-file`), so tokens never live in the TOML itself.
const configPath = process.env.RELAY_CONFIG_FILE ?? "relay.toml";
const relayToml = readFileSync(configPath, "utf8");

const tomlWithoutComments = relayToml
  .split("\n")
  .filter((line) => !line.trimStart().startsWith("#"))
  .join("\n");

const referencedSecrets = [
  ...new Set(
    [
      ...tomlWithoutComments
        .replaceAll("$$", "")
        .matchAll(/\$(?:\{([^}]+)\}|([A-Za-z0-9_]+))/g),
    ].map((m) => m[1] ?? m[2]),
  ),
];

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
        RELAY_CONFIG: Redacted.make(relayToml),
        RELAY_AUTH_TOKEN: Config.Redacted("RELAY_AUTH_TOKEN"),
        ...Object.fromEntries(referencedSecrets.map((name) => [name, Config.Redacted(name)])),
      },
    });

    return { url: relay.url };
  }),
);
