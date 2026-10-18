// SPDX-License-Identifier: GPL-3.0-or-later
import { mount } from "svelte";
import App from "./App.svelte";
import "./lib/theme.css";

// `vite --mode mock` stands in for the Rust side so the UI runs in a plain browser. The
// condition is constant in any other mode, so the mock never reaches a real build.
const ready =
  import.meta.env.MODE === "mock" ? import("./lib/mock").then((m) => m.install()) : Promise.resolve();

ready.then(() => mount(App, { target: document.getElementById("app")! }));
