"use strict";
// Tell the open web app that a sign-in finished, then close the popup.
const result = document.body.dataset.result;
try {
  new BroadcastChannel("endara-oauth").postMessage({ result });
} catch {}
if (result === "ok" && window.opener) setTimeout(() => window.close(), 1200);
