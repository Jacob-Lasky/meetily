"use client"

import { useEffect, useState } from "react"
import { useTheme } from "next-themes"
import { Monitor, Moon, Sun } from "lucide-react"
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
      <div role="radiogroup" aria-label="Appearance" className="flex shrink-0 rounded-md border border-gray-200 bg-gray-50 p-1">
        {OPTIONS.map(({ value, label, icon: Icon }) => {
          const selected = current === value
          return (
            <button
              key={value}
              type="button"
              role="radio"
              aria-checked={selected}
              onClick={() => setTheme(value)}
              className={`flex items-center gap-1.5 rounded px-3 py-1.5 text-sm font-medium transition-colors ${
                selected ? "bg-white text-gray-900 shadow-sm" : "text-gray-600 hover:text-gray-900"
              }`}
            >
              <Icon className="h-4 w-4" />
              {label}
            </button>
          )
        })}
      </div>
    </div>
  )
}
