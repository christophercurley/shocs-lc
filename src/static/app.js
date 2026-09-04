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


function kelvinToCss(kelvin) {
  // Approximate black-body RGB for the swatch when saturation is near zero.
  const temperature = Math.max(1000, Math.min(40000, Number(kelvin || 3500))) / 100;
  let red;
  let green;
  let blue;

  if (temperature <= 66) {
    red = 255;
    green = 99.4708025861 * Math.log(temperature) - 161.1195681661;
    blue = temperature <= 19
      ? 0
      : 138.5177312231 * Math.log(temperature - 10) - 305.0447927307;
  } else {
    red = 329.698727446 * Math.pow(temperature - 60, -0.1332047592);
    green = 288.1221695283 * Math.pow(temperature - 60, -0.0755148492);
    blue = 255;
  }

  const clampByte = (value) => Math.round(Math.max(0, Math.min(255, value)));
  return `rgb(${clampByte(red)}, ${clampByte(green)}, ${clampByte(blue)})`;
}

function colorToCss(hue, saturation, kelvin) {
  if (Number(saturation) <= 1) return kelvinToCss(kelvin);
  return `hsl(${Number(hue)}deg ${Number(saturation)}% 50%)`;
}

function setColorWheelThumb(refs, hue, saturation) {
  const radius = Math.max(0, Math.min(100, Number(saturation))) / 2;
  const angle = (Number(hue) - 90) * Math.PI / 180;
  const x = 50 + Math.cos(angle) * radius;
  const y = 50 + Math.sin(angle) * radius;

  refs.colorThumb.style.left = `${x}%`;
  refs.colorThumb.style.top = `${y}%`;
}

function renderColor(refs, hue, saturation, kelvin) {
  const normalizedHue = ((Number(hue) % 360) + 360) % 360;
  const normalizedSaturation = Math.max(0, Math.min(100, Number(saturation)));

  refs.currentHue = normalizedHue;
  refs.currentSaturation = normalizedSaturation;
  refs.currentKelvin = Number(kelvin || 3500);

  refs.colorSwatch.style.background = colorToCss(
    normalizedHue,
    normalizedSaturation,
    refs.currentKelvin,
  );
  refs.colorSwatchButton.title =
    `${Math.round(normalizedHue)}° · ${Math.round(normalizedSaturation)}% saturation`;

  refs.hueValue.textContent = `${Math.round(normalizedHue)}°`;
  refs.saturationValue.textContent = `${Math.round(normalizedSaturation)}%`;
  refs.colorWheel.setAttribute("aria-valuenow", String(Math.round(normalizedHue)));
  refs.colorWheel.setAttribute(
    "aria-valuetext",
    `${Math.round(normalizedHue)} degrees, ${Math.round(normalizedSaturation)} percent saturation`,
  );
  setColorWheelThumb(refs, normalizedHue, normalizedSaturation);
}

function colorFromPointer(wheel, event) {
  const rect = wheel.getBoundingClientRect();
  const centerX = rect.left + rect.width / 2;
  const centerY = rect.top + rect.height / 2;
  const dx = event.clientX - centerX;
  const dy = event.clientY - centerY;
  const maxRadius = Math.min(rect.width, rect.height) / 2;
  const distance = Math.min(maxRadius, Math.hypot(dx, dy));
  const saturation = maxRadius > 0 ? (distance / maxRadius) * 100 : 0;
  const hue = (Math.atan2(dy, dx) * 180 / Math.PI + 90 + 360) % 360;

  return { hue, saturation };
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

  const colorRow = document.createElement("div");
  colorRow.className = "control-row color-row";
  colorRow.innerHTML = `
    <div class="control-copy">
      <span class="control-label">Color</span>
      <span class="control-help">Tap the color to open live control.</span>
    </div>
  `;

  const colorSwatchButton = document.createElement("button");
  colorSwatchButton.type = "button";
  colorSwatchButton.className = "color-swatch-button";
  colorSwatchButton.setAttribute("aria-label", "Open color picker");
  colorSwatchButton.setAttribute("aria-expanded", "false");

  const colorSwatch = document.createElement("span");
  colorSwatch.className = "color-swatch";
  colorSwatchButton.append(colorSwatch);
  colorRow.append(colorSwatchButton);

  const colorPanel = document.createElement("div");
  colorPanel.className = "color-picker-panel";
  colorPanel.hidden = true;

  const colorWheel = document.createElement("div");
  colorWheel.className = "color-wheel";
  colorWheel.tabIndex = 0;
  colorWheel.setAttribute("role", "slider");
  colorWheel.setAttribute("aria-label", "Hue and saturation");
  colorWheel.setAttribute("aria-valuemin", "0");
  colorWheel.setAttribute("aria-valuemax", "360");

  const colorThumb = document.createElement("span");
  colorThumb.className = "color-thumb";
  colorWheel.append(colorThumb);

  const colorReadout = document.createElement("div");
  colorReadout.className = "color-readout";

  const hueReadout = document.createElement("span");
  hueReadout.innerHTML = `Hue <strong></strong>`;
  const hueValue = hueReadout.querySelector("strong");

  const saturationReadout = document.createElement("span");
  saturationReadout.innerHTML = `Saturation <strong></strong>`;
  const saturationValue = saturationReadout.querySelector("strong");

  colorReadout.append(hueReadout, saturationReadout);

  const colorHelp = document.createElement("p");
  colorHelp.className = "color-picker-help";
  colorHelp.textContent =
    "Drag around the wheel for live hue and saturation. Brightness stays on the slider above.";

  colorPanel.append(colorWheel, colorReadout, colorHelp);

  const colorSender = createLiveControlSender(
    `/api/lights/${encodeURIComponent(light.id)}/color`,
    {
      onError(error) {
        showToast(error.message, true);
      },
      async onCommit() {
        showToast(`${name.textContent}: custom color`);
        await refreshLights();
      },
    },
  );

  function sendColor(final = false) {
    const payload = {
      hue_degrees: Math.round(refs.currentHue),
      saturation_percent: Math.round(refs.currentSaturation),
    };

    refs.lastColorMutationAt = performance.now();
    refs.lastModeMutationAt = performance.now();
    testSwitch.input.checked = false;

    if (final) colorSender.commit(payload);
    else colorSender.update(payload);
  }

  function applyPointerColor(event, final = false) {
    const { hue, saturation } = colorFromPointer(colorWheel, event);
    renderColor(refs, hue, saturation, refs.currentKelvin);
    sendColor(final);
  }

  colorSwatchButton.addEventListener("click", () => {
    const opening = colorPanel.hidden;
    colorPanel.hidden = !opening;
    colorSwatchButton.setAttribute("aria-expanded", String(opening));
    card.classList.toggle("color-picker-open", opening);
  });

  colorWheel.addEventListener("pointerdown", (event) => {
    refs.colorInteracting = true;
    colorWheel.setPointerCapture(event.pointerId);
    applyPointerColor(event, false);
  });

  colorWheel.addEventListener("pointermove", (event) => {
    if (!refs.colorInteracting) return;
    applyPointerColor(event, false);
  });

  colorWheel.addEventListener("pointerup", (event) => {
    if (!refs.colorInteracting) return;
    applyPointerColor(event, true);
    refs.colorInteracting = false;

    if (colorWheel.hasPointerCapture(event.pointerId)) {
      colorWheel.releasePointerCapture(event.pointerId);
    }
  });

  colorWheel.addEventListener("pointercancel", () => {
    refs.colorInteracting = false;
  });

  // Keyboard fallback: arrows change hue; Shift+Up/Down changes saturation.
  colorWheel.addEventListener("keydown", (event) => {
    let hue = refs.currentHue;
    let saturation = refs.currentSaturation;
    const step = event.shiftKey ? 10 : 3;

    if (event.key === "ArrowLeft") hue -= step;
    else if (event.key === "ArrowRight") hue += step;
    else if (event.key === "ArrowUp") saturation += step;
    else if (event.key === "ArrowDown") saturation -= step;
    else return;

    event.preventDefault();
    renderColor(refs, hue, saturation, refs.currentKelvin);
    sendColor(true);
  });

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

  controls.append(powerRow, brightnessRow, colorRow, colorPanel, testRow);
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
    colorRow,
    colorSwatchButton,
    colorSwatch,
    colorPanel,
    colorWheel,
    colorThumb,
    hueValue,
    saturationValue,
    currentHue: Number(light.hue_degrees ?? 0),
    currentSaturation: Number(light.saturation_percent ?? 0),
    currentKelvin: Number(light.kelvin ?? 3500),
    colorInteracting: false,
    lastColorMutationAt: 0,
    testInput: testSwitch.input,
    modePending: false,
    lastModeMutationAt: 0,
  };

  renderColor(
    refs,
    light.hue_degrees ?? 0,
    light.saturation_percent ?? 0,
    light.kelvin ?? 3500,
  );

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

  const hasColor =
    light.hue_degrees !== null &&
    light.saturation_percent !== null &&
    light.kelvin !== null;

  refs.colorRow.classList.toggle("is-disabled", !hasColor);
  refs.colorSwatchButton.disabled = !hasColor;

  if (
    hasColor &&
    !refs.colorInteracting &&
    refreshStartedAt >= refs.lastColorMutationAt
  ) {
    renderColor(
      refs,
      light.hue_degrees,
      light.saturation_percent,
      light.kelvin,
    );
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
