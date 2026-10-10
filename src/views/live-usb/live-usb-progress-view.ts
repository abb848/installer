import { localize } from "../../localization/localize.js";
import {
  installerError,
  type InstallerError,
} from "../../utils/installer-error.js";
import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";
import { wizardState } from "../../state/wizard-state.js";
import {
  createLiveUsb,
  saveLiveIso,
  type FlashProgress,
} from "../../api/index.js";
import { readDriveSelection } from "../../utils/drive-selection.js";
import { InstallDiagnostics } from "../../utils/diagnostics.js";
import "../../components/install-progress.js";

/**
 * Creates the stick or ISO. Fires the same `flash-complete` / `flash-error`
 * events as the regular progress view, so the shell handles retry and going
 * back the same way.
 */
@customElement("live-usb-progress-view")
export class LiveUsbProgressView extends LitElement {
  static styles = css`
    :host {
      display: block;
      height: 100%;
    }
    install-progress {
      width: 100%;
    }
    .keep-plugged-in {
      margin: 0 auto 1rem;
      max-width: 500px;
      padding: 0.75rem 1rem;
      border-radius: 8px;
      text-align: center;
      font-weight: 500;
      color: var(--ha-error-color, #db4437);
      background-color: rgba(219, 68, 55, 0.1);
      border: 1px solid rgba(219, 68, 55, 0.3);
    }
  `;

  @state()
  private _progress: FlashProgress | null = null;

  @state()
  private _error: InstallerError | null = null;

  private _running = false;
  private _stageStartTime: number | null = null;
  private _stageStartBytes = 0;
  private _diagnostics?: InstallDiagnostics;

  retry(): void {
    if (!this._error?.retryable) return;
    this._error = null;
    this._progress = null;
    void this._start();
  }

  connectedCallback() {
    super.connectedCallback();
    void this._start();
  }

  private get _usb(): boolean {
    return wizardState.getState().selections.liveMedia !== "iso";
  }

  private async _start() {
    if (this._running) return;
    this._running = true;
    this._diagnostics = new InstallDiagnostics("flash");
    this._stageStartTime = null;
    this._stageStartBytes = 0;
    const selections = wizardState.getState().selections;
    const onProgress = (progress: FlashProgress) => {
      this._diagnostics?.advance(progress.stage);
      const previous = this._progress;
      // The download stage covers two files; time each one on its own.
      if (
        previous?.stage !== progress.stage ||
        previous.total_bytes !== progress.total_bytes ||
        progress.bytes_processed < previous.bytes_processed
      ) {
        this._stageStartTime = Date.now();
        this._stageStartBytes = progress.bytes_processed;
      }
      this._progress = progress;
    };

    try {
      if (this._usb) {
        const drive = readDriveSelection(selections);
        if (!drive) {
          throw new Error(
            localize("views.live_usb.progress_view.no_stick_selected")
          );
        }
        await createLiveUsb(
          drive.id,
          {
            size: drive.size,
            model: drive.model,
            vendor: drive.vendor,
            serial: drive.serial,
          },
          onProgress
        );
      } else {
        const path = await saveLiveIso(
          selections.liveIsoFolder ?? "",
          selections.liveIsoName ?? "",
          selections.liveIsoOverwrite === true,
          onProgress
        );
        wizardState.setSelection("liveIsoPath", path);
      }
      this.dispatchEvent(
        new CustomEvent("flash-complete", { bubbles: true, composed: true })
      );
    } catch (error) {
      this._diagnostics?.fail(error);
      this._error = installerError(
        error,
        error instanceof Error ? error.message : undefined
      );
      this.dispatchEvent(
        new CustomEvent("flash-error", {
          detail: { retryable: this._error.retryable },
          bubbles: true,
          composed: true,
        })
      );
    } finally {
      this._running = false;
    }
  }

  render() {
    const stages = this._usb
      ? ["downloading", "extracting", "writing", "verifying"]
      : ["downloading", "extracting"];
    const stage = this._progress?.stage || "downloading";
    return html`
      <install-progress
        .stages=${stages.map((id) => ({ id, label: this._describe(id) }))}
        .stage=${stage}
        .stageTitle=${this._title(stage)}
        .description=${this._describe(stage)}
        .progress=${this._progress?.progress || 0}
        .bytesProcessed=${this._progress?.bytes_processed || 0}
        .totalBytes=${this._progress?.total_bytes || 0}
        .stageStartTime=${this._stageStartTime}
        .stageStartBytes=${this._stageStartBytes}
        .indeterminate=${!this._progress?.total_bytes && stage !== "complete"}
        .measurable=${!!this._progress?.total_bytes || stage === "complete"}
        .showUnknownBytes=${true}
        .error=${this._error?.message ?? null}
      >
        ${this._usb && !this._error
          ? html`<p class="keep-plugged-in">
              ${localize("views.live_usb.progress_view.do_not_remove")}
            </p>`
          : ""}
      </install-progress>
    `;
  }

  private _title(stage: string): string {
    switch (stage) {
      case "downloading":
        return localize("views.proxmox.proxmox_progress_view.downloading");
      case "extracting":
        return this._usb
          ? localize("views.live_usb.progress_view.preparing")
          : localize("views.live_usb.progress_view.building");
      case "writing":
        return localize("views.sbc.progress_view.writing");
      case "verifying":
        return localize("views.sbc.progress_view.verifying");
      case "complete":
        return localize("views.proxmox.proxmox_progress_view.complete");
      default:
        return localize("common.error");
    }
  }

  private _describe(stage: string): string {
    switch (stage) {
      case "downloading":
        return this._usb
          ? localize("views.live_usb.progress_view.downloading_usb")
          : localize("views.live_usb.progress_view.downloading_iso");
      case "extracting":
        return this._usb
          ? localize("views.live_usb.progress_view.preparing_description")
          : localize("views.live_usb.progress_view.building_description");
      case "writing":
        return localize("views.live_usb.progress_view.writing_description");
      case "verifying":
        return localize("views.sbc.progress_view.verifying_the_written_data");
      default:
        return localize("views.live_usb.progress_view.creating");
    }
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "live-usb-progress-view": LiveUsbProgressView;
  }
}
