import { LitElement, css, html } from "lit";
import { customElement, property, state } from "lit/decorators.js";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { openExternalUrl } from "../utils/external-url.js";
import {
  getDiagnostics,
  diagnosticText,
  reportUrl,
  type Diagnostics,
} from "../utils/diagnostics.js";
import "@home-assistant/webawesome/dist/components/button/button.js";
import "@home-assistant/webawesome/dist/components/dialog/dialog.js";

@customElement("diagnostics-actions")
export class DiagnosticsActions extends LitElement {
  static styles = css`
    :host {
      display: block;
      margin-top: 1rem;
    }
    wa-dialog {
      --width: 36rem;
    }
    .actions {
      display: flex;
      flex-wrap: wrap;
      gap: 0.5rem;
      margin-top: 1rem;
    }
    textarea {
      box-sizing: border-box;
      width: 100%;
      height: 12rem;
      font: 0.8125rem monospace;
      color: var(--ha-text-color);
      background: var(--ha-background-color);
      border: 1px solid var(--ha-border-color, #ccc);
      border-radius: 4px;
      padding: 0.5rem;
    }
    p {
      line-height: 1.5;
    }
    .version {
      font-size: 0.875rem;
    }
  `;

  @property({ type: Boolean }) about = false;
  @state() private _open = false;
  @state() private _version = "";
  @state() private _data?: Diagnostics;
  @state() private _status = "";

  // Load only when opened: this button sits under every error message, and
  // showing an error should not start native calls of its own
  private async _show() {
    this._open = true;
    this._data = undefined;
    void getVersion()
      .then((version) => {
        this._version ||= version;
      })
      .catch(() => {});
    this._status = "Loading diagnostics...";
    try {
      this._data = await getDiagnostics();
      this._version = this._data.version;
      this._status = "";
    } catch {
      this._status =
        "Diagnostics could not be loaded. Open the logs folder to find the local log.";
    }
  }

  private async _copy() {
    if (!this._data) return;
    try {
      await navigator.clipboard.writeText(diagnosticText(this._data));
      this._status = "Diagnostics copied";
    } catch {
      this._status =
        "Could not copy diagnostics. Select and copy the text below.";
      this.renderRoot.querySelector("textarea")?.select();
    }
  }

  private async _openLogs() {
    try {
      await invoke("open_logs_folder");
      this._status = "Logs folder opened";
    } catch {
      this._status = "Could not open the logs folder.";
    }
  }

  render() {
    const report = this._data ? reportUrl(this._data) : undefined;
    return html`
      <wa-button appearance="plain" @click=${this._show}
        >${this.about ? "About" : "Report a problem"}</wa-button
      >
      <wa-dialog
        label=${this.about
          ? "About Home Assistant Installer"
          : "Report a problem"}
        .open=${this._open}
        @wa-after-hide=${() => {
          this._open = false;
        }}
      >
        <p class="version">
          Home Assistant Installer${this._version ? ` ${this._version}` : ""}
        </p>
        <p>
          GitHub issues are public. Diagnostics omit server addresses,
          usernames, credentials, paths, and raw error details. Review any files
          or screenshots before attaching them.
        </p>
        ${this._data
          ? html`<textarea
              aria-label="Diagnostics"
              readonly
              .value=${diagnosticText(this._data)}
            ></textarea>`
          : ""}
        ${report?.shortened
          ? html`<p>
              The report contains a shortened log. Copy diagnostics for the full
              tail.
            </p>`
          : ""}
        <p role="status">${this._status}</p>
        <div class="actions">
          <wa-button
            variant="brand"
            ?disabled=${!report}
            @click=${() => report && openExternalUrl(report.url)}
            >Report a problem</wa-button
          >
          <wa-button ?disabled=${!this._data} @click=${this._copy}
            >Copy diagnostics</wa-button
          >
          <wa-button @click=${this._openLogs}>Open logs folder</wa-button>
        </div>
      </wa-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "diagnostics-actions": DiagnosticsActions;
  }
}
