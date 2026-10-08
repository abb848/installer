import { LitElement, html, css } from "lit";
import {
  ViewAccessibility,
  reducedMotionStyles,
} from "../../utils/view-accessibility.js";
import { customElement, state } from "lit/decorators.js";
import { wizardState } from "../../state/wizard-state.js";
import { openExternalUrl } from "../../utils/external-url.js";
import "../../components/info-dialog.js";
import "../../components/option-card.js";

@customElement("minipc-setup-method-view")
export class MiniPCSetupMethodView extends LitElement {
  protected readonly _accessibility = new ViewAccessibility(this);
  static styles = css`
    ${reducedMotionStyles}
    :host {
      display: flex;
      flex-direction: column;
      align-items: center;
      height: 100%;
    }

    h2 {
      font-size: 1.5rem;
      font-weight: 400;
      color: var(--ha-text-color, #212121);
      margin: 0 0 0.5rem 0;
      text-align: center;
    }

    .subtitle {
      font-size: 1rem;
      color: var(--ha-secondary-text-color, #727272);
      margin: 0 0 2rem 0;
      text-align: center;
      max-width: 500px;
    }

    .options {
      display: flex;
      flex-direction: column;
      gap: 1rem;
      width: 100%;
      max-width: 500px;
    }
  `;

  @state()
  private _showUsbDialog = false;

  render() {
    return html`
      <h2>How will you install?</h2>
      <p class="subtitle">
        Choose how you want to install Home Assistant on your mini PC
      </p>

      <div class="options">
        <option-card
          horizontal
          title="I can connect the drive"
          description="Connect the SSD or NVMe drive from your mini PC to this computer via USB adapter"
          image="/assets/icons/drive-connect.svg"
          @click=${this._onConnectDrive}
          ><span slot="end" aria-hidden="true">→</span></option-card
        >
        <option-card
          horizontal
          title="I need to boot from USB"
          description="Create a bootable USB drive to install Home Assistant directly on the mini PC"
          image="/assets/icons/usb-boot.svg"
          @click=${this._onUsbBoot}
          ><span slot="end" aria-hidden="true">→</span></option-card
        >
      </div>

      <info-dialog
        ?open=${this._showUsbDialog}
        title="USB boot installation"
        message="Creating bootable USB drives is not supported by this installer. However, we have detailed instructions in our documentation that will guide you through the process."
        primaryLabel="View instructions"
        secondaryLabel="Go back"
        @dialog-primary=${this._onOpenDocs}
        @dialog-secondary=${this._onCloseDialog}
      ></info-dialog>
    `;
  }

  private _onConnectDrive() {
    wizardState.setSelection("installMethod", "direct");
    wizardState.nextStep();
  }

  private _onUsbBoot() {
    this._showUsbDialog = true;
  }

  private _onCloseDialog() {
    this._showUsbDialog = false;
  }

  private async _onOpenDocs() {
    this._showUsbDialog = false;
    await openExternalUrl(
      "https://www.home-assistant.io/installation/generic-x86-64"
    );
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "minipc-setup-method-view": MiniPCSetupMethodView;
  }
}
