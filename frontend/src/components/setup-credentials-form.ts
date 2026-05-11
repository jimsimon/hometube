/**
 * <hometube-setup-credentials-form>
 *
 * Step 2 of the setup wizard. Collects Google Cloud Console credentials
 * (client ID, client secret, YouTube API key) plus the OAuth redirect
 * URI, validates the values via `POST /api/setup/test-credentials`, and
 * — once the parent confirms — persists them with
 * `POST /api/setup/credentials`.
 *
 * Emits a bubbling `setup-credentials-saved` CustomEvent when the values
 * have been persisted, so the wrapping `<hometube-setup-wizard>` can
 * advance to the next step.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";

export interface CredentialsPayload {
  google_client_id: string;
  google_client_secret: string;
  youtube_api_key: string;
  redirect_uri: string;
}

type Status =
  | { kind: "idle" }
  | { kind: "testing" }
  | { kind: "tested-ok" }
  | { kind: "saving" }
  | { kind: "saved" }
  | { kind: "error"; message: string };

@customElement("hometube-setup-credentials-form")
export class SetupCredentialsForm extends LitElement {
  /**
   * Pre-populated redirect URI computed server-side from the request
   * `Host` header. Editable, so the parent can override it (e.g., when
   * accessing through a reverse proxy).
   */
  @property({ type: String, attribute: "suggested-redirect-uri" })
  suggestedRedirectUri = "";

  @state() private status: Status = { kind: "idle" };

  static styles = css`
    :host {
      display: block;
    }
    form {
      display: grid;
      gap: 1rem;
      max-width: 40rem;
    }
    label {
      display: grid;
      gap: 0.25rem;
      font-weight: 500;
    }
    input {
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      font: inherit;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
    }
    .actions {
      display: flex;
      gap: 0.75rem;
      flex-wrap: wrap;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    button.secondary {
      background: transparent;
      color: var(--wa-color-text-normal);
    }
    button[disabled] {
      opacity: 0.5;
      cursor: not-allowed;
    }
    .status {
      font-size: 0.9rem;
    }
    .status.error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
    .status.ok {
      color: var(--wa-color-success-fill, #15803d);
    }
  `;

  private getValues(): CredentialsPayload {
    const root = this.renderRoot as ShadowRoot;
    const v = (id: string) =>
      (root.getElementById(id) as HTMLInputElement | null)?.value.trim() ?? "";
    return {
      google_client_id: v("client-id"),
      google_client_secret: v("client-secret"),
      youtube_api_key: v("api-key"),
      redirect_uri: v("redirect-uri") || this.suggestedRedirectUri,
    };
  }

  private async onTest(e: Event): Promise<void> {
    e.preventDefault();
    this.status = { kind: "testing" };
    try {
      await api.post("/api/setup/test-credentials", this.getValues());
      this.status = { kind: "tested-ok" };
    } catch (err) {
      this.status = { kind: "error", message: this.errorMessage(err) };
    }
  }

  private async onSubmit(e: Event): Promise<void> {
    e.preventDefault();
    this.status = { kind: "saving" };
    const payload = this.getValues();
    try {
      await api.post("/api/setup/credentials", payload);
      this.status = { kind: "saved" };
      this.dispatchEvent(
        new CustomEvent("setup-credentials-saved", {
          bubbles: true,
          composed: true,
          detail: payload,
        }),
      );
    } catch (err) {
      this.status = { kind: "error", message: this.errorMessage(err) };
    }
  }

  private errorMessage(err: unknown): string {
    if (err instanceof ApiError) {
      if (typeof err.body === "string" && err.body.length > 0) return err.body;
      return `Error: ${err.message}`;
    }
    if (err instanceof Error) return err.message;
    return "Unknown error";
  }

  override render() {
    const status = this.status;
    const busy = status.kind === "testing" || status.kind === "saving";

    return html`
      <form @submit=${this.onSubmit} novalidate>
        <p>
          You'll need a Google Cloud project with the
          <a
            href="https://console.cloud.google.com/apis/library/youtube.googleapis.com"
            target="_blank"
            rel="noreferrer noopener"
            >YouTube Data API</a
          >
          enabled and OAuth2 credentials of type "Web application". Add this redirect URI to your
          OAuth client:
          <code>${this.suggestedRedirectUri}</code>.
        </p>

        <label for="client-id">
          Google Client ID
          <input id="client-id" name="client-id" type="text" autocomplete="off" required />
        </label>

        <label for="client-secret">
          Google Client Secret
          <input
            id="client-secret"
            name="client-secret"
            type="password"
            autocomplete="off"
            required
          />
        </label>

        <label for="api-key">
          YouTube Data API Key
          <input id="api-key" name="api-key" type="password" autocomplete="off" required />
        </label>

        <label for="redirect-uri">
          Authorized Redirect URI
          <input
            id="redirect-uri"
            name="redirect-uri"
            type="url"
            .value=${this.suggestedRedirectUri}
            required
          />
        </label>

        <div class="actions">
          <button type="button" class="secondary" ?disabled=${busy} @click=${this.onTest}>
            ${status.kind === "testing" ? "Testing…" : "Test connection"}
          </button>
          <button type="submit" ?disabled=${busy}>
            ${status.kind === "saving" ? "Saving…" : "Save & continue"}
          </button>
        </div>

        ${status.kind === "tested-ok"
          ? html`<p class="status ok" role="status">Looks good — Google is reachable.</p>`
          : null}
        ${status.kind === "saved"
          ? html`<p class="status ok" role="status">Credentials saved.</p>`
          : null}
        ${status.kind === "error"
          ? html`<p class="status error" role="alert">${status.message}</p>`
          : null}
      </form>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-setup-credentials-form": SetupCredentialsForm;
  }
}
