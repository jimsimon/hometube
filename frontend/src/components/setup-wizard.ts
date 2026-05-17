/**
 * <hometube-setup-wizard>
 *
 * Multi-step state machine driving HomeTube's first-time setup flow.
 * The server-rendered page only provides this component plus its
 * children; all step transitions are managed client-side.
 *
 * Steps:
 *   1. Welcome
 *   2. Create parent account (name + PIN)
 *   3. Optional: invite additional accounts
 *   4. Complete (calls `POST /api/setup/complete` and reloads to `/`)
 *
 * On load, the component fetches `/api/setup/status` so a partially-
 * completed wizard resumes at the right step.
 */

import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api, ApiError } from "../services/api.js";

import "./setup-pin-input.js";

type Step = "welcome" | "create-parent" | "invite" | "complete";

interface SetupStatus {
  complete: boolean;
  has_first_parent: boolean;
}

@customElement("hometube-setup-wizard")
export class SetupWizard extends LitElement {
  @state() private step: Step = "welcome";
  @state() private completing = false;
  @state() private completeError = "";

  // Create-parent form state
  @state() private parentName = "";
  @state() private parentPin = "";
  @state() private parentPinConfirm = "";
  @state() private registerBusy = false;
  @state() private registerError = "";

  static styles = css`
    :host {
      display: block;
    }
    .step-nav {
      display: flex;
      gap: 0.5rem;
      flex-wrap: wrap;
      margin-block: 1rem 1.5rem;
      list-style: none;
      padding: 0;
    }
    .step-nav li {
      padding: 0.25rem 0.75rem;
      border-radius: 999rem;
      background: var(--wa-color-surface-raised, #f3f4f6);
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet, #6b7280);
    }
    .step-nav li[aria-current="step"] {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font-weight: 600;
    }
    .actions {
      display: flex;
      gap: 0.75rem;
      flex-wrap: wrap;
      margin-top: 1.5rem;
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
    .status.error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
    form {
      display: grid;
      gap: 1rem;
      max-width: 24rem;
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
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refreshStatus();
  }

  private async refreshStatus(): Promise<void> {
    try {
      const status = await api.get<SetupStatus>("/api/setup/status");

      // Pick the step with the lowest unfinished prerequisite.
      if (status.complete) {
        this.step = "complete";
      } else if (!status.has_first_parent) {
        // No parent yet — stay on welcome or create-parent (don't
        // regress from create-parent back to welcome on refresh).
        if (this.step === "complete" || this.step === "invite") {
          this.step = "welcome";
        }
      } else {
        // Parent exists but setup not yet marked complete — go to the
        // family/invite step.
        this.step = "invite";
      }
    } catch (err) {
      this.completeError = `Could not load setup status: ${this.errorMessage(err)}`;
    }
  }

  private errorMessage(err: unknown): string {
    if (err instanceof ApiError) {
      if (typeof err.body === "string" && err.body.length > 0) return err.body;
      return err.message;
    }
    if (err instanceof Error) return err.message;
    return "Unknown error";
  }

  private async onRegisterParent(e: Event): Promise<void> {
    e.preventDefault();
    const name = this.parentName.trim();
    if (!name) {
      this.registerError = "Name is required.";
      return;
    }
    if (this.parentPin.length < 4 || this.parentPin.length > 6) {
      this.registerError = "PIN must be 4-6 digits.";
      return;
    }
    if (this.parentPin !== this.parentPinConfirm) {
      this.registerError = "PINs do not match.";
      return;
    }
    this.registerBusy = true;
    this.registerError = "";
    try {
      await api.post("/api/auth/register", {
        display_name: name,
        pin: this.parentPin,
        role: "parent",
      });
      this.step = "invite";
      void this.refreshStatus();
    } catch (err) {
      this.registerError = this.errorMessage(err);
    } finally {
      this.registerBusy = false;
    }
  }

  private async finish(): Promise<void> {
    this.completing = true;
    this.completeError = "";
    try {
      await api.post("/api/setup/complete");
      window.location.href = "/";
    } catch (err) {
      this.completeError = this.errorMessage(err);
      this.completing = false;
    }
  }

  private renderStepNav() {
    const steps: { key: Step; label: string }[] = [
      { key: "welcome", label: "Welcome" },
      { key: "create-parent", label: "Parent" },
      { key: "invite", label: "Family" },
      { key: "complete", label: "Done" },
    ];
    return html`
      <ol class="step-nav" aria-label="Setup progress">
        ${steps.map(
          (s) => html`<li aria-current=${this.step === s.key ? "step" : "false"}>${s.label}</li>`,
        )}
      </ol>
    `;
  }

  override render() {
    return html` ${this.renderStepNav()} ${this.renderStep()} `;
  }

  private renderStep() {
    switch (this.step) {
      case "welcome":
        return html`
          <section aria-labelledby="welcome-heading">
            <h2 id="welcome-heading">Let's get you set up</h2>
            <p>
              You'll create a parent profile with a name and a PIN. The PIN is used to sign in and
              protects parent access.
            </p>
            <div class="actions">
              <button @click=${() => (this.step = "create-parent")}>Begin</button>
            </div>
          </section>
        `;

      case "create-parent":
        return html`
          <section aria-labelledby="parent-heading">
            <h2 id="parent-heading">Create your parent profile</h2>
            <form @submit=${this.onRegisterParent} novalidate>
              <label>
                Your name
                <input
                  type="text"
                  autocomplete="name"
                  required
                  .value=${this.parentName}
                  @input=${(e: Event) => (this.parentName = (e.target as HTMLInputElement).value)}
                />
              </label>
              <label>
                PIN (4-6 digits)
                <input
                  type="password"
                  inputmode="numeric"
                  pattern="[0-9]*"
                  minlength="4"
                  maxlength="6"
                  autocomplete="new-password"
                  required
                  .value=${this.parentPin}
                  @input=${(e: Event) => (this.parentPin = (e.target as HTMLInputElement).value)}
                />
              </label>
              <label>
                Confirm PIN
                <input
                  type="password"
                  inputmode="numeric"
                  pattern="[0-9]*"
                  minlength="4"
                  maxlength="6"
                  autocomplete="new-password"
                  required
                  .value=${this.parentPinConfirm}
                  @input=${(e: Event) =>
                    (this.parentPinConfirm = (e.target as HTMLInputElement).value)}
                />
              </label>
              ${this.registerError
                ? html`<p class="status error" role="alert">${this.registerError}</p>`
                : null}
              <div class="actions">
                <button type="submit" ?disabled=${this.registerBusy}>
                  ${this.registerBusy ? "Creating…" : "Create profile"}
                </button>
                <button
                  type="button"
                  class="secondary"
                  @click=${() => (this.step = "welcome")}
                  ?disabled=${this.registerBusy}
                >
                  Back
                </button>
              </div>
            </form>
          </section>
        `;

      case "invite":
        return html`
          <section aria-labelledby="invite-heading">
            <h2 id="invite-heading">Add more family members (optional)</h2>
            <p>
              Add additional parents or children now, or skip ahead and add them later from the
              parent dashboard.
            </p>
            <div class="actions">
              <button ?disabled=${this.completing} @click=${() => this.finish()}>
                ${this.completing ? "Finishing…" : "Finish setup"}
              </button>
            </div>
            ${this.completeError
              ? html`<p class="status error" role="alert">${this.completeError}</p>`
              : null}
          </section>
        `;

      case "complete":
        return html`
          <section aria-labelledby="complete-heading">
            <h2 id="complete-heading">All set</h2>
            <p>Setup is complete. You can now use HomeTube.</p>
            <div class="actions">
              <a href="/"><button>Go to home</button></a>
            </div>
          </section>
        `;
    }
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-setup-wizard": SetupWizard;
  }
}
