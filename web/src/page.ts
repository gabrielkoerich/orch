import { LitElement } from "lit";
import { REFRESH } from "./shared";

// A page that loads its own data and loads it again when orch reports a change
export abstract class Page extends LitElement {
  static override properties = {
    error: { state: true },
    loaded: { state: true },
  };

  declare error: string;
  declare loaded: boolean;

  constructor() {
    super();
    this.error = "";
    this.loaded = false;
  }

  override createRenderRoot(): HTMLElement {
    return this;
  }

  private readonly onRefresh = (): void => {
    void this.reload();
  };

  override connectedCallback(): void {
    super.connectedCallback();
    window.addEventListener(REFRESH, this.onRefresh);
    void this.reload();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    window.removeEventListener(REFRESH, this.onRefresh);
  }

  protected async reload(): Promise<void> {
    try {
      await this.load();
      this.error = "";
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    }

    this.loaded = true;
  }

  protected abstract load(): Promise<void>;
}
