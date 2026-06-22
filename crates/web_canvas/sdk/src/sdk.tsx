// Canvas SDK — bundled by esbuild into `../assets/canvas-sdk.js` and embedded in
// the `web_canvas` crate. It provides the React runtime, shadcn/ui components,
// Recharts, and lucide icons as globals so an agent-authored `Canvas()` can use
// them without imports. Rebuild with `npm run build` after editing.

import React from "react";
import { createRoot } from "react-dom/client";
import * as Recharts from "recharts";
import {
  Activity,
  AlertCircle,
  AlertTriangle,
  ArrowDownRight,
  ArrowUpRight,
  Check,
  CheckCircle2,
  ChevronRight,
  CircleDot,
  Info,
  Minus,
  Plus,
  TrendingDown,
  TrendingUp,
  X,
  XCircle,
} from "lucide-react";

import { cn } from "./lib/utils";
import { Button, buttonVariants } from "./components/ui/button";
import {
  Card,
  CardHeader,
  CardFooter,
  CardTitle,
  CardDescription,
  CardContent,
} from "./components/ui/card";
import { Badge, badgeVariants } from "./components/ui/badge";
import {
  Table,
  TableHeader,
  TableBody,
  TableFooter,
  TableHead,
  TableRow,
  TableCell,
  TableCaption,
} from "./components/ui/table";
import { Alert, AlertTitle, AlertDescription } from "./components/ui/alert";
import { Separator } from "./components/ui/separator";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "./components/ui/tabs";

// --- Host theme ---------------------------------------------------------------

function useHostTheme() {
  const t = (typeof window !== "undefined" && (window as any).__zedTheme) || null;
  const prefersDark =
    window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
  return {
    kind: t ? t.kind : prefersDark ? "dark" : "light",
    name: t ? t.name : null,
    colors: (t && t.colors) || {},
  };
}

// --- Layout helpers (not part of shadcn; thin Tailwind wrappers) --------------

function Page({ children, prose = true, className }: any) {
  return (
    <div
      className={cn(
        "mx-auto max-w-3xl",
        prose && "prose dark:prose-invert prose-headings:font-serif",
        className,
      )}
    >
      {children}
    </div>
  );
}

function Stack({ children, gap = 4, className }: any) {
  return <div className={cn("flex flex-col not-prose", "gap-" + gap, className)}>{children}</div>;
}

function Row({ children, gap = 4, className }: any) {
  return (
    <div className={cn("flex flex-row items-center flex-wrap not-prose", "gap-" + gap, className)}>
      {children}
    </div>
  );
}

function Grid({ children, cols = 2, gap = 4, className }: any) {
  return (
    <div className={cn("grid not-prose", "grid-cols-" + cols, "gap-" + gap, className)}>
      {children}
    </div>
  );
}

// --- Globals ------------------------------------------------------------------

const icons = {
  Activity,
  AlertCircle,
  AlertTriangle,
  ArrowDownRight,
  ArrowUpRight,
  Check,
  CheckCircle2,
  ChevronRight,
  CircleDot,
  Info,
  Minus,
  Plus,
  TrendingDown,
  TrendingUp,
  X,
  XCircle,
};

Object.assign(window as any, {
  React,
  ReactDOM: { createRoot },
  Recharts,
  // Common Recharts primitives, also exposed directly for convenience.
  ResponsiveContainer: Recharts.ResponsiveContainer,
  BarChart: Recharts.BarChart,
  Bar: Recharts.Bar,
  LineChart: Recharts.LineChart,
  Line: Recharts.Line,
  AreaChart: Recharts.AreaChart,
  Area: Recharts.Area,
  PieChart: Recharts.PieChart,
  Pie: Recharts.Pie,
  Cell: Recharts.Cell,
  XAxis: Recharts.XAxis,
  YAxis: Recharts.YAxis,
  CartesianGrid: Recharts.CartesianGrid,
  Tooltip: Recharts.Tooltip,
  Legend: Recharts.Legend,
  // Utilities + layout
  cn,
  useHostTheme,
  Page,
  Stack,
  Row,
  Grid,
  // shadcn/ui
  Button,
  buttonVariants,
  Card,
  CardHeader,
  CardFooter,
  CardTitle,
  CardDescription,
  CardContent,
  Badge,
  badgeVariants,
  Table,
  TableHeader,
  TableBody,
  TableFooter,
  TableHead,
  TableRow,
  TableCell,
  TableCaption,
  Alert,
  AlertTitle,
  AlertDescription,
  Separator,
  Tabs,
  TabsList,
  TabsTrigger,
  TabsContent,
  // lucide icons (namespace + commonly used ones inline)
  Icons: icons,
  ...icons,
});
