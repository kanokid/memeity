import { mount } from "svelte";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import App from "./App.svelte";
import "./app.css";

export default mount(App, { target: document.getElementById("app")! });
