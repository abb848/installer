import { expect, fixtureSync, html } from "@open-wc/testing";
import { wizardState } from "../../../../src/state/wizard-state.js";
import "../../../../src/views/utm/utm-check-view.js";
import { mockTauriIpc, restoreTauriIpc, settle } from "../../tauri-ipc.js";

describe("utm-check-view", () => {
  beforeEach(() => {
    wizardState.startFlow("vm");
  });
  afterEach(() => {
    wizardState.reset();
    restoreTauriIpc();
  });

  it("keeps Next gated after a string rejection and supports checking again", async () => {
    let attempts = 0;
    mockTauriIpc((cmd) => {
      expect(cmd).to.equal("check_utm_status");
      return ++attempts === 1
        ? Promise.reject("Status check failed")
        : { installed: true, path: "/Applications/UTM.app", version: "4.5.0" };
    });
    const el = fixtureSync(html`<utm-check-view></utm-check-view>`);
    await settle();
    expect(el.shadowRoot!.textContent).to.contain("Error checking UTM");
    expect(wizardState.getState().selections.utmInstalled).to.be.false;
    const retry = el.shadowRoot!.querySelector("wa-button")!;
    expect(retry.textContent).to.contain("Try again");
    retry.click();
    await settle();
    expect(attempts).to.equal(2);
    expect(wizardState.getState().selections.utmInstalled).to.be.true;
    expect(el.shadowRoot!.textContent).to.contain(
      "Ready to create a Home Assistant virtual machine"
    );
  });

  it("keeps a missing UTM installation gated and offers its download", async () => {
    mockTauriIpc((cmd) => {
      expect(cmd).to.equal("check_utm_status");
      return { installed: false, path: null, version: null };
    });
    const el = fixtureSync(html`<utm-check-view></utm-check-view>`);
    await settle();
    expect(wizardState.getState().selections.utmInstalled).to.be.false;
    expect(el.shadowRoot!.textContent).to.contain("Download UTM");
    expect(el.shadowRoot!.textContent).to.contain("not installed");
  });
});
