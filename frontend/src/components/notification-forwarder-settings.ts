/**
 * <hometube-notification-forwarder-settings>
 *
 * Parent-only Notifications card. Lets the parent choose a self-hosted
 * push provider (Apprise, ntfy.sh, or Gotify), configure its connection
 * details, pick which notification types should be forwarded, and send
 * a synthetic test notification.
 *
 * Secrets (ntfy token, Gotify app token, Apprise basic-auth password)
 * are redacted by the backend to the literal string "********". The
 * component re-sends that placeholder back on save when the user
 * doesn't edit the secret, and the backend replaces it with the
 * previously stored value before persisting.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api } from "../services/api.js";

type Provider = "ntfy" | "gotify" | "apprise";

interface NtfyConfig {
  provider: "ntfy";
  base_url: string;
  topic: string;
  token?: string | null;
  priority?: number | null;
}

interface GotifyConfig {
  provider: "gotify";
  base_url: string;
  app_token: string;
  priority?: number | null;
}

interface AppriseConfig {
  provider: "apprise";
  base_url: string;
  config_key?: string | null;
  urls?: string | null;
  basic_auth_user?: string | null;
  basic_auth_password?: string | null;
}

type ForwarderConfig = NtfyConfig | GotifyConfig | AppriseConfig;

interface ForwardingSettings {
  enabled: boolean;
  provider: ForwarderConfig | null;
  enabled_types: string[];
}

interface ConfigResponse {
  settings: ForwardingSettings;
  known_types: string[];
}

const TYPE_LABELS: Record<string, string> = {
  ytdlp_failure: "yt-dlp failures",
  new_search_term: "New search term",
  system_update: "System updates",
};

@customElement("hometube-notification-forwarder-settings")
export class NotificationForwarderSettings extends LitElement {
  @state() private settings: ForwardingSettings | null = null;
  @state() private knownTypes: string[] = [];
  @state() private busy = false;
  @state() private status = "";
  @state() private statusKind: "ok" | "err" | "" = "";

  static styles = css`
    :host {
      display: block;
      margin-bottom: 1rem;
    }
    article {
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    .row {
      display: grid;
      grid-template-columns: max-content 1fr;
      gap: 0.5rem 1rem;
      align-items: center;
      margin: 0.5rem 0;
    }
    label {
      font-weight: 600;
    }
    input[type="text"],
    input[type="password"],
    input[type="number"],
    input[type="url"],
    select,
    textarea {
      width: 100%;
      padding: 0.4rem 0.5rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
      box-sizing: border-box;
    }
    textarea {
      min-height: 4rem;
      font-family: monospace;
    }
    fieldset {
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.375rem;
      padding: 0.5rem 0.75rem;
      margin: 0.75rem 0;
    }
    legend {
      font-weight: 600;
      padding: 0 0.25rem;
    }
    .types {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(15rem, 1fr));
      gap: 0.25rem 1rem;
    }
    .types label {
      display: flex;
      align-items: center;
      gap: 0.4rem;
      font-weight: 400;
    }
    .actions {
      display: flex;
      gap: 0.5rem;
      flex-wrap: wrap;
      margin-top: 1rem;
    }
    button {
      padding: 0.4rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    button.primary {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      border-color: transparent;
    }
    .status {
      margin-top: 0.5rem;
      font-size: 0.9rem;
    }
    .status.ok {
      color: var(--wa-color-success-text, #166534);
    }
    .status.err {
      color: var(--wa-color-danger-text, #b91c1c);
    }
    .hint {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
      margin: 0.25rem 0 0;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    try {
      const resp = await api.get<ConfigResponse>("/api/notifications/config");
      this.settings = resp.settings;
      this.knownTypes = resp.known_types;
      // Ensure provider has correct shape when null.
      if (!this.settings.provider) {
        this.settings = { ...this.settings, provider: null };
      }
    } catch (err) {
      this.setStatus(`Failed to load: ${(err as Error).message}`, "err");
    }
  }

  private setStatus(msg: string, kind: "ok" | "err" | "" = ""): void {
    this.status = msg;
    this.statusKind = kind;
  }

  private onEnabledChange(e: Event): void {
    if (!this.settings) return;
    const enabled = (e.target as HTMLInputElement).checked;
    this.settings = { ...this.settings, enabled };
  }

  private onProviderChange(e: Event): void {
    if (!this.settings) return;
    const value = (e.target as HTMLSelectElement).value as Provider | "";
    let provider: ForwarderConfig | null = null;
    if (value === "ntfy") {
      provider = {
        provider: "ntfy",
        base_url: "https://ntfy.sh",
        topic: "",
        token: "",
      };
    } else if (value === "gotify") {
      provider = {
        provider: "gotify",
        base_url: "",
        app_token: "",
      };
    } else if (value === "apprise") {
      provider = {
        provider: "apprise",
        base_url: "",
        config_key: "",
        urls: "",
      };
    }
    this.settings = { ...this.settings, provider };
  }

  private updateProvider(patch: Partial<ForwarderConfig>): void {
    if (!this.settings?.provider) return;
    this.settings = {
      ...this.settings,
      provider: { ...this.settings.provider, ...patch } as ForwarderConfig,
    };
  }

  private onTypeToggle(type: string, checked: boolean): void {
    if (!this.settings) return;
    const set = new Set(this.settings.enabled_types);
    if (checked) set.add(type);
    else set.delete(type);
    this.settings = { ...this.settings, enabled_types: [...set] };
  }

  private async onSave(): Promise<void> {
    if (!this.settings) return;
    this.busy = true;
    this.setStatus("");
    try {
      const resp = await api.put<ConfigResponse>("/api/notifications/config", this.settings);
      this.settings = resp.settings;
      this.knownTypes = resp.known_types;
      this.setStatus("Saved.", "ok");
    } catch (err) {
      this.setStatus(`Save failed: ${(err as Error).message}`, "err");
    } finally {
      this.busy = false;
    }
  }

  private async onTest(): Promise<void> {
    this.busy = true;
    this.setStatus("");
    try {
      const resp = await api.post<{ ok: boolean; error?: string }>(
        "/api/notifications/config/test",
        { notification_type: "system_update" },
      );
      if (resp.ok) {
        this.setStatus("Test notification sent.", "ok");
      } else {
        this.setStatus(`Test failed: ${resp.error ?? "unknown error"}`, "err");
      }
    } catch (err) {
      this.setStatus(`Test failed: ${(err as Error).message}`, "err");
    } finally {
      this.busy = false;
    }
  }

  private renderProviderFields() {
    const p = this.settings?.provider;
    if (!p) return nothing;
    if (p.provider === "ntfy") {
      return html`
        <div class="row">
          <label for="ntfy-url">Base URL</label>
          <input
            id="ntfy-url"
            type="url"
            .value=${p.base_url}
            @input=${(e: Event) =>
              this.updateProvider({ base_url: (e.target as HTMLInputElement).value })}
          />
          <label for="ntfy-topic">Topic</label>
          <input
            id="ntfy-topic"
            type="text"
            .value=${p.topic}
            @input=${(e: Event) =>
              this.updateProvider({ topic: (e.target as HTMLInputElement).value })}
          />
          <label for="ntfy-token">Access token</label>
          <input
            id="ntfy-token"
            type="password"
            autocomplete="off"
            .value=${p.token ?? ""}
            placeholder="(optional)"
            @input=${(e: Event) =>
              this.updateProvider({ token: (e.target as HTMLInputElement).value })}
          />
          <label for="ntfy-priority">Default priority</label>
          <select
            id="ntfy-priority"
            @change=${(e: Event) =>
              this.updateProvider({
                priority: parseInt((e.target as HTMLSelectElement).value, 10) || null,
              })}
          >
            <option value="">(auto by type)</option>
            ${[1, 2, 3, 4, 5].map(
              (v) => html`<option value=${v} ?selected=${p.priority === v}>${v}</option>`,
            )}
          </select>
        </div>
        <p class="hint">
          Leave the token blank for public topics on ntfy.sh. For self-hosted servers, paste a
          bearer access token.
        </p>
      `;
    }
    if (p.provider === "gotify") {
      return html`
        <div class="row">
          <label for="gotify-url">Base URL</label>
          <input
            id="gotify-url"
            type="url"
            .value=${p.base_url}
            placeholder="https://gotify.example.com"
            @input=${(e: Event) =>
              this.updateProvider({ base_url: (e.target as HTMLInputElement).value })}
          />
          <label for="gotify-token">Application token</label>
          <input
            id="gotify-token"
            type="password"
            autocomplete="off"
            .value=${p.app_token}
            @input=${(e: Event) =>
              this.updateProvider({ app_token: (e.target as HTMLInputElement).value })}
          />
          <label for="gotify-priority">Default priority</label>
          <select
            id="gotify-priority"
            @change=${(e: Event) => {
              const v = (e.target as HTMLSelectElement).value;
              this.updateProvider({ priority: v === "" ? null : parseInt(v, 10) });
            }}
          >
            <option value="">(auto by type)</option>
            ${[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10].map(
              (v) => html`<option value=${v} ?selected=${p.priority === v}>${v}</option>`,
            )}
          </select>
        </div>
      `;
    }
    // Apprise
    return html`
      <div class="row">
        <label for="apprise-url">Apprise API base URL</label>
        <input
          id="apprise-url"
          type="url"
          .value=${p.base_url}
          placeholder="http://apprise:8000"
          @input=${(e: Event) =>
            this.updateProvider({ base_url: (e.target as HTMLInputElement).value })}
        />
        <label for="apprise-key">Stateful config key</label>
        <input
          id="apprise-key"
          type="text"
          .value=${p.config_key ?? ""}
          placeholder="(optional, e.g. 'family')"
          @input=${(e: Event) =>
            this.updateProvider({ config_key: (e.target as HTMLInputElement).value })}
        />
        <label for="apprise-urls">Stateless URLs</label>
        <textarea
          id="apprise-urls"
          placeholder="One URL per line, e.g. mailto://user:pass@example.com"
          .value=${p.urls ?? ""}
          @input=${(e: Event) =>
            this.updateProvider({ urls: (e.target as HTMLTextAreaElement).value })}
        ></textarea>
        <label for="apprise-user">Basic auth user</label>
        <input
          id="apprise-user"
          type="text"
          .value=${p.basic_auth_user ?? ""}
          placeholder="(optional)"
          @input=${(e: Event) =>
            this.updateProvider({ basic_auth_user: (e.target as HTMLInputElement).value })}
        />
        <label for="apprise-pass">Basic auth password</label>
        <input
          id="apprise-pass"
          type="password"
          autocomplete="off"
          .value=${p.basic_auth_password ?? ""}
          @input=${(e: Event) =>
            this.updateProvider({ basic_auth_password: (e.target as HTMLInputElement).value })}
        />
      </div>
      <p class="hint">
        Provide either a stateful config key (saved server-side via apprise-api) or one or more
        stateless URLs.
      </p>
    `;
  }

  override render() {
    if (!this.settings) return html`<p>Loading…</p>`;
    const providerKey = this.settings.provider?.provider ?? "";
    return html`
      <article>
        <div class="row">
          <label for="enabled">Enable forwarding</label>
          <input
            id="enabled"
            type="checkbox"
            .checked=${this.settings.enabled}
            @change=${this.onEnabledChange}
          />
          <label for="provider">Provider</label>
          <select id="provider" .value=${providerKey} @change=${this.onProviderChange}>
            <option value="" ?selected=${providerKey === ""}>None</option>
            <option value="ntfy" ?selected=${providerKey === "ntfy"}>ntfy.sh</option>
            <option value="gotify" ?selected=${providerKey === "gotify"}>Gotify</option>
            <option value="apprise" ?selected=${providerKey === "apprise"}>Apprise</option>
          </select>
        </div>

        ${this.renderProviderFields()}

        <fieldset>
          <legend>Forward these notification types</legend>
          <div class="types">
            ${this.knownTypes.map((t) => {
              const checked = this.settings!.enabled_types.includes(t);
              return html`<label>
                <input
                  type="checkbox"
                  .checked=${checked}
                  @change=${(e: Event) =>
                    this.onTypeToggle(t, (e.target as HTMLInputElement).checked)}
                />
                ${TYPE_LABELS[t] ?? t}
              </label>`;
            })}
          </div>
        </fieldset>

        <div class="actions">
          <button type="button" class="primary" ?disabled=${this.busy} @click=${this.onSave}>
            Save
          </button>
          <button
            type="button"
            ?disabled=${this.busy || !this.settings.provider}
            @click=${this.onTest}
          >
            Send test notification
          </button>
        </div>
        <div class="status ${this.statusKind}" role="status" aria-live="polite">${this.status}</div>
      </article>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-notification-forwarder-settings": NotificationForwarderSettings;
  }
}
