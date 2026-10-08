import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";
import { wizardState, type WizardState } from "../../state/wizard-state.js";
import { openExternalLink } from "../../utils/external-url.js";

import "../../components/install-success.js";

const INSTALLATION_GUIDES: Record<string, string> = {
  "rpi3-64": "https://www.home-assistant.io/installation/raspberrypi/",
  "rpi4-64": "https://www.home-assistant.io/installation/raspberrypi/",
  "rpi5-64": "https://www.home-assistant.io/installation/raspberrypi/",
  "odroid-c2": "https://www.home-assistant.io/installation/odroid/",
  "odroid-c4": "https://www.home-assistant.io/installation/odroid/",
  "odroid-m1": "https://www.home-assistant.io/installation/odroid/",
  "odroid-n2":
    "https://www.home-assistant.io/installation/odroid/#flashing-an-odroid-n2",
  "odroid-m1s":
    "https://www.home-assistant.io/installation/odroid/#flashing-an-odroid-m1s",
  "generic-x86-64":
    "https://www.home-assistant.io/installation/generic-x86-64/",
  "generic-aarch64":
    "https://developers.home-assistant.io/docs/operating-system/boards/generic-aarch64/",
};

@customElement("success-view")
export class SuccessView extends LitElement {
  static styles = css`
    :host {
      display: block;
      height: 100%;
    }
  `;

  @state()
  private _wizardState: WizardState = wizardState.getState();

  private _unsubscribe?: () => void;

  connectedCallback() {
    super.connectedCallback();
    this._unsubscribe = wizardState.subscribe((state) => {
      this._wizardState = state;
    });
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this._unsubscribe?.();
  }

  render() {
    const deviceName =
      (this._wizardState.selections.deviceName as string) || "your device";
    const board = this._wizardState.selections.deviceConfig?.board ?? "";
    const isMiniPc = this._wizardState.currentFlow === "minipc";
    const supportsDirectUsb = board === "odroid-n2" || board === "odroid-m1s";
    const guide = Object.prototype.hasOwnProperty.call(
      INSTALLATION_GUIDES,
      board
    )
      ? INSTALLATION_GUIDES[board]
      : "https://www.home-assistant.io/installation/";

    return html`
      <install-success
        .subtitle=${"Home Assistant OS has been written to your storage device"}
        .notice=${html`Do not format or initialize the written drive. Choose
        Cancel if offered, or Ignore or Eject on macOS. Your computer may not
        recognize its Home Assistant partitions.`}
        .steps=${[
          html`If the drive is still listed on your computer, use your operating
          system's eject option before disconnecting it.`,
          isMiniPc
            ? html`Install or reconnect the written drive in your mini PC.
              Select that drive in the firmware boot order, with UEFI boot
              enabled and Secure Boot disabled.`
            : supportsDirectUsb
              ? html`If you used a storage adapter, insert the written media
                into ${deviceName}. If you flashed the board directly over USB,
                disconnect the USB and power cables.
                ${board === "odroid-m1s"
                  ? html`With the board powered off, remove the EMMC2UMS SD card
                    if you used one.`
                  : html`With the board powered off, set the boot mode switch
                    back to MMC as described in the installation guide below.`}`
              : html`Insert the written storage into ${deviceName}.`,
          html`Connect an Ethernet cable to the same network as your computer,
          with internet access. Then connect power to start the device.`,
          html`Open
            <a
              href="http://homeassistant.local:8123"
              target="_blank"
              rel="noopener noreferrer"
              @click=${(event: Event) =>
                openExternalLink(event, "http://homeassistant.local:8123")}
              >homeassistant.local:8123</a
            >
            in your browser. If it still does not open after a few minutes, find
            the device's IP address in your router or on an attached display,
            then open <code>http://&lt;IP address&gt;:8123</code>.`,
          html`The Preparing Home Assistant page downloads the latest Home
          Assistant. Allow about 20 minutes, depending on your internet
          connection. Keep power and Ethernet connected until the welcome screen
          appears.`,
        ]}
        .footer=${html`<a
          href=${guide}
          target="_blank"
          rel="noopener noreferrer"
          @click=${(event: Event) => openExternalLink(event, guide)}
          >${guide === "https://www.home-assistant.io/installation/"
            ? "Installation guide"
            : `${deviceName} installation guide`}</a
        >`}
      ></install-success>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "success-view": SuccessView;
  }
}
