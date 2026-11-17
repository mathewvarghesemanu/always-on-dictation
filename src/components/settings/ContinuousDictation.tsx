import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { Slider } from "../ui/Slider";
import { ShortcutInput } from "./ShortcutInput";
import { useSettings } from "../../hooks/useSettings";

interface ContinuousDictationProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

/**
 * Always-on dictation's toggle, plus its pause length and shortcut once it
 * is turned on.
 */
export const ContinuousDictation: React.FC<ContinuousDictationProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, resetSetting, isUpdating } = useSettings();
  const enabled = getSetting("continuous_dictation_enabled") ?? false;
  const pauseMs = getSetting("continuous_dictation_pause_ms") ?? 2000;

  return (
    <>
      <ToggleSwitch
        checked={enabled}
        onChange={(enabled) =>
          updateSetting("continuous_dictation_enabled", enabled)
        }
        isUpdating={isUpdating("continuous_dictation_enabled")}
        label={t("settings.advanced.continuousDictation.title")}
        description={t("settings.advanced.continuousDictation.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
      {enabled && (
        <>
          <Slider
            value={pauseMs}
            onChange={(value) =>
              updateSetting("continuous_dictation_pause_ms", value)
            }
            onReset={() => resetSetting("continuous_dictation_pause_ms")}
            isResetting={isUpdating("continuous_dictation_pause_ms")}
            min={500}
            max={10000}
            step={250}
            label={t("settings.advanced.continuousDictation.pause.title")}
            description={t(
              "settings.advanced.continuousDictation.pause.description",
            )}
            descriptionMode={descriptionMode}
            grouped={grouped}
            formatValue={(v) =>
              t("settings.advanced.continuousDictation.pause.value", {
                seconds: (v / 1000).toFixed(2).replace(/\.?0+$/, ""),
              })
            }
          />
          <ShortcutInput
            shortcutId="continuous_dictation"
            descriptionMode={descriptionMode}
            grouped={grouped}
          />
        </>
      )}
    </>
  );
};
