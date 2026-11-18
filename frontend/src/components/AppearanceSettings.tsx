"use client"

import { useEffect, useState } from "react"
import { useTheme } from "next-themes"
import { Monitor, Moon, Sun } from "lucide-react"
import { RadioGroup } from "radix-ui"
import type { ThemePreference } from "./ThemeProvider"

const OPTIONS: { value: ThemePreference; label: string; icon: typeof Sun }[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
]

export function AppearanceSettings() {
  const { theme, setTheme } = useTheme()
  // next-themes only knows the stored preference after mount; rendering the
  // selection before then would mismatch the static HTML.
  const [mounted, setMounted] = useState(false)
  useEffect(() => setMounted(true), [])
  const current = mounted ? theme : undefined

  return (
    <div className="flex items-center justify-between gap-4">
      <div>
        <h3 className="text-lg font-semibold text-gray-900 mb-2">Appearance</h3>
        <p className="text-sm text-gray-600">Follow your system setting, or always use light or dark</p>
      </div>
      {/* Radix supplies radio semantics: one Tab stop, arrow keys move and select. */}
      <RadioGroup.Root
        aria-label="Appearance"
        value={current ?? ""}
        onValueChange={(value) => setTheme(value)}
        orientation="horizontal"
        className="flex shrink-0 rounded-md border border-gray-200 bg-gray-50 p-1"
      >
        {OPTIONS.map(({ value, label, icon: Icon }) => (
          <RadioGroup.Item
            key={value}
            value={value}
            className="flex items-center gap-1.5 rounded px-3 py-1.5 text-sm font-medium text-gray-600 transition-colors hover:text-gray-900 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500 data-[state=checked]:bg-white data-[state=checked]:text-gray-900 data-[state=checked]:shadow-sm"
          >
            <Icon className="h-4 w-4" />
            {label}
          </RadioGroup.Item>
        ))}
      </RadioGroup.Root>
    </div>
  )
}
