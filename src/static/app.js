const grid = document.querySelector("#light-grid");
const refreshStatus = document.querySelector("#refresh-status");
const toast = document.querySelector("#toast");
const cards = new Map();
let toastTimer;

function showToast(message, isError = false) {
  clearTimeout(toastTimer);
  toast.textContent = message;
  toast.classList.toggle("is-error", isError);
  toast.classList.add("is-visible");
  toastTimer = setTimeout(() => toast.classList.remove("is-visible"), 2600);
}

async function api(url, options = {}) {
  const response = await fetch(url, {
    ...options,
    headers: {
      "Content-Type": "application/json",
      ...(options.headers || {}),
    },
  });

  if (!response.ok) {
    let message = `Request failed (${response.status})`;
    try {
      const body = await response.json();
      if (body.error) message = body.error;
    } catch (_) {
      // Keep the status-based fallback when the body is not JSON.
    }
    throw new Error(message);
  }

  return response;
}

function createLiveControlSender(url, { onError, onCommit }) {
  const intervalMs = 100; // Cap live control traffic at about 10 Hz.
  let pending = null;
  let inFlight = false;
  let timer = null;
  let lastSentAt = 0;

  async function transmit() {
    if (inFlight || pending === null) return;

    const payload = pending;
    pending = null;
    inFlight = true;
    lastSentAt = performance.now();

    try {
      await api(url, {
        method: "PUT",
        body: JSON.stringify(payload),
      });

      if (payload.final && onCommit) onCommit(payload);
    } catch (error) {
      onError(error);
    } finally {
      inFlight = false;
      if (pending !== null) schedule();
    }
  }

  function schedule() {
    if (inFlight || timer !== null || pending === null) return;

    const elapsed = performance.now() - lastSentAt;
    const delay = Math.max(0, intervalMs - elapsed);
    timer = setTimeout(() => {
      timer = null;
      transmit();
    }, delay);
  }

  return {
    update(payload) {
      // Latest-wins coalescing prevents a fast drag from building a backlog.
      pending = { ...payload, final: false };
      schedule();
    },

    commit(payload) {
      pending = { ...payload, final: true };

      if (!inFlight) {
        if (timer !== null) {
          clearTimeout(timer);
          timer = null;
        }
        transmit();
      }
    },
  };
}

function setBrightnessVisual(slider, percent) {
  const clamped = Math.max(0, Math.min(100, Number(percent)));
  slider.style.setProperty("--brightness-fill", `${clamped}%`);
}

function renderBrightness(refs, percent) {
  const clamped = Math.max(0, Math.min(100, Number(percent)));
  refs.slider.value = String(clamped);
  refs.brightnessValue.textContent = `${Math.round(clamped)}%`;
  setBrightnessVisual(refs.slider, clamped);
}

function cancelBrightnessAnimation(refs) {
  if (refs.brightnessAnimationFrame !== null) {
    cancelAnimationFrame(refs.brightnessAnimationFrame);
    refs.brightnessAnimationFrame = null;
  }
}

function animateBrightness(refs, fromPercent, toPercent, remainingMs) {
  cancelBrightnessAnimation(refs);

  const duration = Math.max(0, Number(remainingMs));
  const from = Number(fromPercent);
  const to = Number(toPercent);

  if (duration <= 0 || Math.abs(to - from) < 0.01) {
    renderBrightness(refs, to);
    return;
  }

  const startedAt = performance.now();

  function frame(now) {
    if (refs.brightnessInteracting) {
      refs.brightnessAnimationFrame = null;
      return;
    }

    const progress = Math.min(1, (now - startedAt) / duration);
    renderBrightness(refs, from + (to - from) * progress);

    if (progress < 1) {
      refs.brightnessAnimationFrame = requestAnimationFrame(frame);
    } else {
      refs.brightnessAnimationFrame = null;
    }
  }

  refs.brightnessAnimationFrame = requestAnimationFrame(frame);
}

function createSwitch(checked, label, onChange) {
  const wrapper = document.createElement("label");
  wrapper.className = "switch";
  wrapper.setAttribute("aria-label", label);

  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = checked;

  const track = document.createElement("span");
  track.className = "switch-track";

  input.addEventListener("change", () => onChange(input));
  wrapper.append(input, track);

  return { wrapper, input };
}

function createCard(light) {
  const card = document.createElement("article");
  card.className = "light-card";
  card.dataset.id = light.id;

  const header = document.createElement("div");
  header.className = "light-header";

  const title = document.createElement("div");
  title.className = "light-title";
  const name = document.createElement("h2");
  const meta = document.createElement("div");
  meta.className = "light-meta";
  title.append(name, meta);

  const badge = document.createElement("div");
  badge.className = "online-badge";
  const dot = document.createElement("span");
  dot.className = "status-dot";
  const badgeText = document.createElement("span");
  badge.append(dot, badgeText);

  header.append(title, badge);

  const controls = document.createElement("div");
  controls.className = "control-list";

  const powerRow = document.createElement("div");
  powerRow.className = "control-row";
  powerRow.innerHTML = `
    <div class="control-copy">
      <span class="control-label">Power</span>
      <span class="control-help">Power override keeps the current mode active.</span>
    </div>
  `;

  let refs;

  const powerSwitch = createSwitch(Boolean(light.power_on), "Power", async (input) => {
    const desired = input.checked;
    refs.powerPending = true;
    refs.lastPowerMutationAt = performance.now();
    input.disabled = true;

    try {
      await api(`/api/lights/${encodeURIComponent(light.id)}/power`, {
        method: "PUT",
        body: JSON.stringify({ on: desired }),
      });
      showToast(`${name.textContent}: ${desired ? "on" : "off"}`);
    } catch (error) {
      input.checked = !desired;
      showToast(error.message, true);
    } finally {
      refs.powerPending = false;
      input.disabled = false;
      await refreshLights();
    }
  });
  powerRow.append(powerSwitch.wrapper);

  const brightnessRow = document.createElement("div");
  brightnessRow.className = "brightness-row";

  const brightnessHeading = document.createElement("div");
  brightnessHeading.className = "brightness-heading";
  brightnessHeading.innerHTML = `
    <div class="control-copy">
      <span class="control-label">Brightness</span>
      <span class="control-help">Live control · updates up to 10 times per second.</span>
    </div>
  `;
  const brightnessValue = document.createElement("span");
  brightnessValue.className = "brightness-value";
  brightnessHeading.append(brightnessValue);

  const slider = document.createElement("input");
  slider.className = "brightness-slider";
  slider.type = "range";
  slider.min = "0";
  slider.max = "100";
  slider.step = "1";
  slider.setAttribute("aria-label", "Brightness");

  const brightnessSender = createLiveControlSender(
    `/api/lights/${encodeURIComponent(light.id)}/brightness`,
    {
      onError(error) {
        showToast(error.message, true);
      },
      async onCommit(payload) {
        showToast(`${name.textContent}: brightness ${payload.percent}%`);
        await refreshLights();
      },
    },
  );

  slider.addEventListener("pointerdown", () => {
    refs.brightnessInteracting = true;
    cancelBrightnessAnimation(refs);
  });

  slider.addEventListener("input", () => {
    refs.brightnessInteracting = true;
    cancelBrightnessAnimation(refs);

    const desired = Math.round(Number(slider.value));
    renderBrightness(refs, desired);

    // Manual brightness still means the user is taking direct control.
    testSwitch.input.checked = false;
    brightnessSender.update({ percent: desired });
  });

  slider.addEventListener("change", () => {
    const desired = Math.round(Number(slider.value));
    refs.brightnessInteracting = false;
    renderBrightness(refs, desired);
    brightnessSender.commit({ percent: desired });
  });

  slider.addEventListener("pointerup", () => {
    refs.brightnessInteracting = false;
  });
  slider.addEventListener("pointercancel", () => {
    refs.brightnessInteracting = false;
  });
  slider.addEventListener("blur", () => {
    refs.brightnessInteracting = false;
  });

  brightnessRow.append(brightnessHeading, slider);

  const testRow = document.createElement("div");
  testRow.className = "control-row";
  testRow.innerHTML = `
    <div class="control-copy">
      <span class="control-label">Test Mode</span>
      <span class="control-help">Color heartbeat + 02:00 / 10:00 power schedule.</span>
    </div>
  `;

  const testSwitch = createSwitch(light.mode === "test", "Test Mode", async (input) => {
    const desired = input.checked;
    refs.modePending = true;
    refs.lastModeMutationAt = performance.now();
    input.disabled = true;

    try {
      await api(`/api/lights/${encodeURIComponent(light.id)}/mode`, {
        method: "PUT",
        body: JSON.stringify({ test: desired }),
      });
      showToast(`${name.textContent}: ${desired ? "Test" : "Custom"} mode`);
    } catch (error) {
      input.checked = !desired;
      showToast(error.message, true);
    } finally {
      refs.modePending = false;
      input.disabled = false;
      await refreshLights();
    }
  });
  testRow.append(testSwitch.wrapper);

  controls.append(powerRow, brightnessRow, testRow);
  card.append(header, controls);

  refs = {
    card,
    name,
    meta,
    badge,
    badgeText,
    powerInput: powerSwitch.input,
    powerPending: false,
    lastPowerMutationAt: 0,
    brightnessRow,
    brightnessValue,
    slider,
    brightnessInteracting: false,
    brightnessAnimationFrame: null,
    testInput: testSwitch.input,
    modePending: false,
    lastModeMutationAt: 0,
  };

  cards.set(light.id, refs);
  grid.append(card);
  return refs;
}

function updateCard(light, refreshStartedAt) {
  const refs = cards.get(light.id) || createCard(light);
  refs.name.textContent = light.label;
  refs.meta.textContent = `${light.id} · ${light.address}`;

  refs.badge.dataset.online = String(light.online);
  refs.badgeText.textContent = light.online ? "Online" : "Offline";

  const hasState = light.power_on !== null && light.brightness_percent !== null;
  refs.powerInput.disabled = !hasState || refs.powerPending;

  // Ignore GET responses that were already in flight when the user clicked.
  // This prevents the classic OFF -> ON -> OFF visual bounce.
  if (
    hasState &&
    !refs.powerPending &&
    refreshStartedAt >= refs.lastPowerMutationAt
  ) {
    refs.powerInput.checked = light.power_on;
  }

  refs.brightnessRow.classList.toggle("is-disabled", !hasState);
  refs.slider.disabled = !hasState;

  if (!hasState) {
    cancelBrightnessAnimation(refs);
    refs.brightnessValue.textContent = "—";
    setBrightnessVisual(refs.slider, 0);
  } else if (!refs.brightnessInteracting) {
    const transition = light.brightness_transition;
    if (transition && transition.remaining_ms > 0) {
      animateBrightness(
        refs,
        light.brightness_percent,
        transition.to_percent,
        transition.remaining_ms,
      );
    } else {
      cancelBrightnessAnimation(refs);
      renderBrightness(refs, light.brightness_percent);
    }
  }

  if (
    !refs.modePending &&
    refreshStartedAt >= refs.lastModeMutationAt
  ) {
    refs.testInput.checked = light.mode === "test";
  }
}

async function refreshLights() {
  const refreshStartedAt = performance.now();

  try {
    const response = await fetch("/api/lights", { cache: "no-store" });
    if (!response.ok) throw new Error(`Could not load lights (${response.status})`);

    const lights = await response.json();
    const seen = new Set(lights.map((light) => light.id));

    document.querySelector("#loading-card")?.remove();

    for (const light of lights) updateCard(light, refreshStartedAt);

    for (const [id, refs] of cards) {
      if (!seen.has(id)) {
        cancelBrightnessAnimation(refs);
        refs.card.remove();
        cards.delete(id);
      }
    }

    if (lights.length === 0 && !document.querySelector("#no-lights")) {
      const empty = document.createElement("div");
      empty.className = "empty-card";
      empty.id = "no-lights";
      empty.textContent = "No LIFX lights discovered yet.";
      grid.append(empty);
    } else if (lights.length > 0) {
      document.querySelector("#no-lights")?.remove();
    }

    refreshStatus.textContent = `${lights.length} light${lights.length === 1 ? "" : "s"} known · live`;
  } catch (error) {
    refreshStatus.textContent = "Controller unavailable";
    showToast(error.message, true);
  }
}

refreshLights();
setInterval(refreshLights, 3000);
