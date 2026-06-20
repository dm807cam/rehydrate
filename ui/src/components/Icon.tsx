import type { SVGProps } from "react";

/* Hand-crafted monochrome icons. All inherit `currentColor`. Designed
 * at 16px, drawn on a 16-unit grid, 1.75 stroke. Inline SVG keeps the
 * bundle tiny (no icon font, no JS lib). */

type IconName =
  | "library"
  | "clock"
  | "notebook"
  | "pdf"
  | "epub"
  | "trash"
  | "folder"
  | "import"
  | "sync"
  | "plug"
  | "unplug"
  | "more"
  | "history"
  | "delete"
  | "restore"
  | "search"
  | "check"
  | "warn"
  | "info"
  | "arrowDown"
  | "arrowUp"
  | "tablet"
  | "selectMode"
  | "checkbox"
  | "checkboxChecked"
  | "checkboxIndeterminate"
  | "settings"
  | "x"
  | "wand";

interface Props extends SVGProps<SVGSVGElement> {
  name: IconName;
  size?: number;
}

export function Icon({ name, size = 14, className, ...rest }: Props) {
  const cls = ["icon-svg", className].filter(Boolean).join(" ");
  const common = {
    viewBox: "0 0 16 16",
    width: size,
    height: size,
    className: cls,
    "aria-hidden": true as const,
    ...rest,
  };
  switch (name) {
    case "library":
      return (
        <svg {...common}>
          <rect x="2" y="3" width="3" height="10" rx="0.6" />
          <rect x="6.5" y="3" width="3" height="10" rx="0.6" />
          <path d="M11 4.4 L13.6 4 L14 13 L11.4 13.4 Z" />
        </svg>
      );
    case "clock":
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="5.5" />
          <path d="M8 4.5 V8 L10.5 9.5" />
        </svg>
      );
    case "notebook":
      return (
        <svg {...common}>
          <rect x="3" y="2.5" width="9" height="11" rx="1" />
          <path d="M5.5 2.5 V13.5" />
          <path d="M7.5 5.5 H10.5 M7.5 8 H10.5 M7.5 10.5 H10" />
        </svg>
      );
    case "pdf":
      return (
        <svg {...common}>
          <path d="M4 2.5 H10 L12.5 5 V13 A0.5 0.5 0 0 1 12 13.5 H4 A0.5 0.5 0 0 1 3.5 13 V3 A0.5 0.5 0 0 1 4 2.5 Z" />
          <path d="M10 2.5 V5 H12.5" />
        </svg>
      );
    case "epub":
      return (
        <svg {...common}>
          <path d="M2.5 4 C5 3, 7 3, 8 4.2 C9 3, 11 3, 13.5 4 V12.5 C11 11.5, 9 11.5, 8 12.7 C7 11.5, 5 11.5, 2.5 12.5 Z" />
          <path d="M8 4.2 V12.7" />
        </svg>
      );
    case "trash":
      return (
        <svg {...common}>
          <path d="M3 4.5 H13" />
          <path d="M5.5 4.5 V3.5 A1 1 0 0 1 6.5 2.5 H9.5 A1 1 0 0 1 10.5 3.5 V4.5" />
          <path d="M4.5 4.5 L5.2 13 A1 1 0 0 0 6.2 14 H9.8 A1 1 0 0 0 10.8 13 L11.5 4.5" />
        </svg>
      );
    case "folder":
      return (
        <svg {...common}>
          <path d="M2.5 5 A0.5 0.5 0 0 1 3 4.5 H6.5 L7.8 6 H13 A0.5 0.5 0 0 1 13.5 6.5 V12.5 A0.5 0.5 0 0 1 13 13 H3 A0.5 0.5 0 0 1 2.5 12.5 Z" />
        </svg>
      );
    case "import":
      return (
        <svg {...common}>
          <path d="M8 2.5 V10 M5 7 L8 10 L11 7" />
          <path d="M3 11 V13 H13 V11" />
        </svg>
      );
    case "sync":
      return (
        <svg {...common}>
          <path d="M3 8 A5 5 0 0 1 13 8" />
          <path d="M11 5 H13.5 V2.5" />
          <path d="M13 8 A5 5 0 0 1 3 8" />
          <path d="M5 11 H2.5 V13.5" />
        </svg>
      );
    case "plug":
      return (
        <svg {...common}>
          <path d="M6 2.5 V5 M10 2.5 V5" />
          <path d="M4.5 5 H11.5 V8 A3.5 3.5 0 0 1 4.5 8 Z" />
          <path d="M8 11.5 V13.5" />
        </svg>
      );
    case "unplug":
      return (
        <svg {...common}>
          <path d="M3 13 L6.5 9.5 M9.5 6.5 L13 3" />
          <path d="M5.5 11 L8 13.5 M2.5 8 L5 10.5" />
          <path d="M11 5 L13.5 7.5 M8 2.5 L10.5 5" />
        </svg>
      );
    case "more":
      return (
        <svg {...common}>
          <circle cx="4" cy="8" r="1" fill="currentColor" stroke="none" />
          <circle cx="8" cy="8" r="1" fill="currentColor" stroke="none" />
          <circle cx="12" cy="8" r="1" fill="currentColor" stroke="none" />
        </svg>
      );
    case "history":
      return (
        <svg {...common}>
          <path d="M3.5 8 A4.5 4.5 0 1 0 5 4.5" />
          <path d="M3 3 V5.5 H5.5" />
          <path d="M8 5 V8 L10 9.5" />
        </svg>
      );
    case "delete":
      return (
        <svg {...common}>
          <path d="M3.5 4.5 H12.5" />
          <path d="M5 7 V12 M8 7 V12 M11 7 V12" />
          <path d="M4.5 4.5 L5.2 13 A1 1 0 0 0 6.2 14 H9.8 A1 1 0 0 0 10.8 13 L11.5 4.5" />
        </svg>
      );
    case "restore":
      return (
        <svg {...common}>
          <path d="M3 8 A5 5 0 1 0 5.5 4" />
          <path d="M3 3.5 V6 H5.5" />
        </svg>
      );
    case "search":
      return (
        <svg {...common}>
          <circle cx="7" cy="7" r="4" />
          <path d="M10 10 L13.5 13.5" />
        </svg>
      );
    case "check":
      return (
        <svg {...common}>
          <path d="M3 8.5 L6.5 12 L13 4.5" />
        </svg>
      );
    case "warn":
      return (
        <svg {...common}>
          <path d="M8 2.5 L14 13 H2 Z" />
          <path d="M8 6.5 V9.5 M8 11 V11.6" />
        </svg>
      );
    case "info":
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="5.5" />
          <path d="M8 7 V11 M8 5 V5.6" />
        </svg>
      );
    case "arrowDown":
      return (
        <svg {...common}>
          <path d="M8 3 V13 M4 9 L8 13 L12 9" />
        </svg>
      );
    case "arrowUp":
      return (
        <svg {...common}>
          <path d="M8 13 V3 M4 7 L8 3 L12 7" />
        </svg>
      );
    case "tablet":
      return (
        <svg {...common}>
          <rect x="3.5" y="2" width="9" height="12" rx="1" />
          <path d="M7 12 H9" />
        </svg>
      );
    case "selectMode":
      return (
        <svg {...common}>
          <rect x="2.5" y="2.5" width="11" height="11" rx="1.5" />
          <path d="M5 8.5 L7.5 11 L11.5 6" />
        </svg>
      );
    case "checkbox":
      return (
        <svg {...common}>
          <rect x="2.5" y="2.5" width="11" height="11" rx="2" />
        </svg>
      );
    case "checkboxChecked":
      return (
        <svg {...common}>
          <rect x="2.5" y="2.5" width="11" height="11" rx="2" fill="currentColor" stroke="none" />
          <path d="M5 8.5 L7.5 11 L11.5 6" stroke="white" strokeWidth="2" />
        </svg>
      );
    case "checkboxIndeterminate":
      return (
        <svg {...common}>
          <rect x="2.5" y="2.5" width="11" height="11" rx="2" fill="currentColor" stroke="none" />
          <path d="M5 8 L11 8" stroke="white" strokeWidth="2" strokeLinecap="round" />
        </svg>
      );
    case "settings":
      // Cogwheel: 8-tooth gear outline + central hole. The path
      // traces alternating tip/valley vertices around a circle
      // (tip radius 6.2, valley radius 4.7) — 24 points, closed.
      return (
        <svg {...common}>
          <path d="M14.1 6.8 L14.1 9.2 L12.3 9.8 L13.2 11.5 L11.5 13.2 L9.8 12.3 L9.2 14.1 L6.8 14.1 L6.2 12.3 L4.5 13.2 L2.8 11.5 L3.7 9.8 L1.9 9.2 L1.9 6.8 L3.7 6.2 L2.8 4.5 L4.5 2.8 L6.2 3.7 L6.8 1.9 L9.2 1.9 L9.8 3.7 L11.5 2.8 L13.2 4.5 L12.3 6.2 Z" />
          <circle cx="8" cy="8" r="2" />
        </svg>
      );
    case "x":
      return (
        <svg {...common}>
          <path d="M4 4 L12 12 M12 4 L4 12" />
        </svg>
      );
    case "wand":
      // Magic-wand glyph used for the "Convert to text" OCR action.
      return (
        <svg {...common}>
          <path d="M3 13 L11 5" />
          <path d="M10 3 L12 5 L14 3 M11 5 L13 7 M9 1 L11 3" />
        </svg>
      );
  }
}
