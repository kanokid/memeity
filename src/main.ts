import { mount } from "svelte";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import App from "./App.svelte";
import Settings from "./Settings.svelte";
import "./app.css";

// Both windows load the same bundle; the window's label decides what it shows.
const Root = getCurrentWebviewWindow().label === "settings" ? Settings : App;

export default mount(Root, { target: document.getElementById("app")! });
