import React from "react";
import ReactDOM from "react-dom/client";
import { Toaster } from "sonner";
import { ThemeProvider } from "@/components/ThemeProvider";
import { App } from "@/App";
import "@/index.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ThemeProvider>
      <App />
      <Toaster position="bottom-center" theme="dark" richColors closeButton />
    </ThemeProvider>
  </React.StrictMode>,
);
