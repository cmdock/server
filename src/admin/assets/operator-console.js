"use strict";

const sessionStatus = document.querySelector("#session-status");
const tokenInput = document.querySelector("#operator-token");
const userIdInput = document.querySelector("#user-id");
const serverStatusOutput = document.querySelector("#server-status");
const statsOutput = document.querySelector("#user-stats");
const syncIdentityOutput = document.querySelector("#user-sync-identity");
const runtimePolicyOutput = document.querySelector("#user-runtime-policy");
const devicesOutput = document.querySelector("#user-devices");
const runtimePolicyForm = document.querySelector("#runtime-policy-form");
const tokenForm = document.querySelector("#token-form");
const userForm = document.querySelector("#user-form");

const TOKEN_KEY = "cmdock.operatorToken";
const USER_KEY = "cmdock.operatorUserId";

function getToken() {
  return window.sessionStorage.getItem(TOKEN_KEY) || "";
}

function setToken(token) {
  if (token) {
    window.sessionStorage.setItem(TOKEN_KEY, token);
  } else {
    window.sessionStorage.removeItem(TOKEN_KEY);
  }
  syncSessionBanner();
}

function getUserId() {
  return window.sessionStorage.getItem(USER_KEY) || "";
}

function setUserId(userId) {
  if (userId) {
    window.sessionStorage.setItem(USER_KEY, userId);
  } else {
    window.sessionStorage.removeItem(USER_KEY);
  }
}

function syncSessionBanner(message = "") {
  const hasToken = Boolean(getToken());
  tokenInput.value = getToken();

  sessionStatus.classList.remove("error");
  if (message) {
    sessionStatus.textContent = message;
    return;
  }

  sessionStatus.textContent = hasToken
    ? "Operator token loaded in this browser session."
    : "No operator token in this browser session.";
}

function pretty(value) {
  return JSON.stringify(value, null, 2);
}

function requireToken() {
  const token = getToken().trim();
  if (!token) {
    sessionStatus.textContent = "Enter the operator token before calling /admin/*.";
    sessionStatus.classList.add("error");
    throw new Error("missing operator token");
  }
  return token;
}

async function adminRequest(path, options = {}) {
  const token = requireToken();
  const headers = new Headers(options.headers || {});
  headers.set("Authorization", `Bearer ${token}`);

  if (options.body && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }

  const response = await fetch(path, {
    method: options.method || "GET",
    headers,
    body: options.body,
  });

  const text = await response.text();
  if (!response.ok) {
    throw new Error(`${response.status} ${text || response.statusText}`);
  }

  if (!text) {
    return null;
  }

  try {
    return JSON.parse(text);
  } catch (_error) {
    return text;
  }
}

async function loadServerStatus() {
  serverStatusOutput.textContent = "Loading...";
  try {
    const payload = await adminRequest("/admin/status");
    serverStatusOutput.textContent = pretty(payload);
    syncSessionBanner();
  } catch (error) {
    serverStatusOutput.textContent = error.message;
    syncSessionBanner(error.message);
    sessionStatus.classList.add("error");
  }
}

async function loadUserPanel(kind) {
  const userId = getUserId().trim();
  if (!userId) {
    throw new Error("Enter a user ID first.");
  }

  const outputs = {
    stats: statsOutput,
    "sync-identity": syncIdentityOutput,
    "runtime-policy": runtimePolicyOutput,
    devices: devicesOutput,
  };

  const output = outputs[kind];
  output.textContent = "Loading...";

  const paths = {
    stats: `/admin/user/${encodeURIComponent(userId)}/stats`,
    "sync-identity": `/admin/user/${encodeURIComponent(userId)}/sync-identity`,
    "runtime-policy": `/admin/user/${encodeURIComponent(userId)}/runtime-policy`,
    devices: `/admin/user/${encodeURIComponent(userId)}/devices`,
  };

  try {
    const payload = await adminRequest(paths[kind]);
    output.textContent = pretty(payload);
    syncSessionBanner();
    return payload;
  } catch (error) {
    output.textContent = error.message;
    syncSessionBanner(error.message);
    sessionStatus.classList.add("error");
    throw error;
  }
}

async function refreshUser() {
  await Promise.allSettled([
    loadUserPanel("stats"),
    loadUserPanel("sync-identity"),
    loadUserPanel("runtime-policy"),
    loadUserPanel("devices"),
  ]);
}

tokenForm.addEventListener("submit", (event) => {
  event.preventDefault();
  setToken(tokenInput.value.trim());
});

document.querySelector("#clear-token").addEventListener("click", () => {
  setToken("");
  serverStatusOutput.textContent = "No data loaded yet.";
});

document.querySelector("#check-status").addEventListener("click", async () => {
  try {
    await loadServerStatus();
  } catch (_error) {
    // loadServerStatus already updates the UI
  }
});

userForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  setUserId(userIdInput.value.trim());
  try {
    await refreshUser();
  } catch (_error) {
    // individual loaders already update the UI
  }
});

document.querySelector("#refresh-user").addEventListener("click", async () => {
  try {
    await refreshUser();
  } catch (_error) {
    // individual loaders already update the UI
  }
});

document.querySelectorAll("[data-load]").forEach((button) => {
  button.addEventListener("click", async () => {
    const kind = button.getAttribute("data-load");
    try {
      await loadUserPanel(kind);
    } catch (_error) {
      // loadUserPanel already updates the UI
    }
  });
});

document.querySelector("#ensure-sync-identity").addEventListener("click", async () => {
  const userId = getUserId().trim();
  if (!userId) {
    syncSessionBanner("Enter a user ID first.");
    sessionStatus.classList.add("error");
    return;
  }

  syncIdentityOutput.textContent = "Ensuring canonical sync identity...";
  try {
    const payload = await adminRequest(
      `/admin/user/${encodeURIComponent(userId)}/sync-identity/ensure`,
      { method: "POST" },
    );
    syncIdentityOutput.textContent = pretty(payload);
    syncSessionBanner("Canonical sync identity ensured.");
  } catch (error) {
    syncIdentityOutput.textContent = error.message;
    syncSessionBanner(error.message);
    sessionStatus.classList.add("error");
  }
});

runtimePolicyForm.addEventListener("submit", async (event) => {
  event.preventDefault();

  const userId = getUserId().trim();
  if (!userId) {
    syncSessionBanner("Enter a user ID first.");
    sessionStatus.classList.add("error");
    return;
  }

  const policyVersion = document.querySelector("#policy-version").value.trim();
  if (!policyVersion) {
    syncSessionBanner("Policy version is required.");
    sessionStatus.classList.add("error");
    return;
  }

  runtimePolicyOutput.textContent = "Applying runtime policy...";

  try {
    const payload = await adminRequest(
      `/admin/user/${encodeURIComponent(userId)}/runtime-policy`,
      {
        method: "PUT",
        body: JSON.stringify({
          policyVersion,
          policy: {
            runtimeAccess: document.querySelector("#runtime-access").value,
            deleteAction: document.querySelector("#delete-action").value,
          },
        }),
      },
    );
    runtimePolicyOutput.textContent = pretty(payload);
    syncSessionBanner("Runtime policy applied.");
  } catch (error) {
    runtimePolicyOutput.textContent = error.message;
    syncSessionBanner(error.message);
    sessionStatus.classList.add("error");
  }
});

tokenInput.value = getToken();
userIdInput.value = getUserId();
syncSessionBanner();
