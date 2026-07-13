import type { Metadata } from "next";
import "./globals.css";
import {
  Cinzel,
  Cormorant_Garamond,
  IBM_Plex_Mono,
  IBM_Plex_Sans,
} from "next/font/google";
import { Sidebar } from "@/components/sidebar";
import { QueryProvider } from "./query-provider";
import { WebGLBackground } from "@/components/baroque/webgl-background";
import { TooltipProvider } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

// "Midnight Baroque" type system: ornate serif display, engraved-caps
// wordmark, and the IBM Plex family for technical body/mono coherence.
const display = Cormorant_Garamond({
  subsets: ["latin"],
  weight: ["400", "500", "600", "700"],
  style: ["normal", "italic"],
  variable: "--font-display",
});
const wordmark = Cinzel({
  subsets: ["latin"],
  weight: ["600", "700"],
  variable: "--font-wordmark",
});
const sans = IBM_Plex_Sans({
  subsets: ["latin"],
  weight: ["400", "500", "600"],
  variable: "--font-sans",
});
const mono = IBM_Plex_Mono({
  subsets: ["latin"],
  weight: ["400", "500"],
  variable: "--font-mono",
});

export const metadata: Metadata = {
  title: "KGateway",
  description: "High-performance AI/LLM gateway — Rust + Next.js",
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html
      lang="en"
      suppressHydrationWarning
      className={cn(
        "dark font-sans",
        display.variable,
        wordmark.variable,
        sans.variable,
        mono.variable,
      )}
    >
      <head>
        {/* Apply the persisted theme before first paint (default: dark).
            Mirrors THEME_STORAGE_KEY in components/baroque/use-theme.ts. */}
        <script
          dangerouslySetInnerHTML={{
            __html:
              '(function(){try{if(localStorage.getItem("kgateway_theme")==="light")document.documentElement.classList.remove("dark")}catch(e){}})()',
          }}
        />
      </head>
      <body>
        {/* z-0: ambient WebGL silk · z-[1]: damask texture · z-10: app shell */}
        <WebGLBackground />
        <div aria-hidden className="bg-damask pointer-events-none fixed inset-0 z-[1] opacity-[0.04]" />
        <QueryProvider>
          <TooltipProvider>
            <div className="relative z-10 flex h-screen">
              <Sidebar />
              <main className="flex-1 overflow-y-auto">
                <div className="mx-auto max-w-5xl p-8">{children}</div>
              </main>
            </div>
          </TooltipProvider>
        </QueryProvider>
      </body>
    </html>
  );
}
