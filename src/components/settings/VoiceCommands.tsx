import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { VoiceCommand } from "@/bindings";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";
import { useShortcutCapture } from "../../hooks/useShortcutCapture";
import { usePressAway } from "../../hooks/usePressAway";
import {
  formatKeyCombination,
  spaceDoesNotClick,
} from "../../lib/utils/keyboard";
import { Input } from "../ui/Input";
import { Button } from "../ui/Button";
import { SettingContainer } from "../ui/SettingContainer";

interface VoiceCommandsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

const newId = () =>
  typeof crypto !== "undefined" && "randomUUID" in crypto
    ? crypto.randomUUID()
    : `vc-${Date.now()}-${Math.random().toString(36).slice(2)}`;

interface RowProps {
  command: VoiceCommand;
  disabled: boolean;
  onChange: (next: VoiceCommand) => void;
  onRemove: () => void;
}

/** One phrase → keys mapping. Click the keys to record new ones. */
const VoiceCommandRow: React.FC<RowProps> = ({
  command,
  disabled,
  onChange,
  onRemove,
}) => {
  const { t } = useTranslation();
  const osType = useOsType();
  const [phrase, setPhrase] = useState(command.phrase);
  useEffect(() => setPhrase(command.phrase), [command.phrase]);

  const { capturing, partial, start, cancel } = useShortcutCapture(
    `voice_command_${command.id}`,
    osType,
    {
      inert: true,
      onCapture: (keys) => onChange({ ...command, keys }),
    },
  );
  const keysRef = useRef<HTMLButtonElement | null>(null);
  usePressAway(capturing, keysRef, cancel);

  const commitPhrase = () => {
    const trimmed = phrase.replace(/\s+/g, " ").trim();
    if (trimmed !== command.phrase) onChange({ ...command, phrase: trimmed });
  };

  const endOnly = command.end_only ?? true;

  return (
    <div className="flex flex-wrap items-center gap-2 px-4 py-2">
      <Input
        type="text"
        className="flex-1 min-w-32"
        value={phrase}
        onChange={(e) => setPhrase(e.target.value)}
        onBlur={commitPhrase}
        onKeyDown={(e) => {
          if (e.key === "Enter") (e.target as HTMLInputElement).blur();
        }}
        placeholder={t("settings.advanced.voiceCommands.phrasePlaceholder")}
        aria-label={t("settings.advanced.voiceCommands.phraseLabel")}
        variant="compact"
        disabled={disabled}
      />
      <button
        ref={keysRef}
        type="button"
        {...spaceDoesNotClick}
        onClick={() => {
          if (!capturing) start();
        }}
        disabled={disabled}
        aria-label={t("settings.advanced.voiceCommands.keysLabel")}
        className={
          capturing
            ? "min-w-24 px-2 py-1 text-sm font-semibold border border-logo-primary bg-logo-primary/20 rounded-md"
            : "min-w-24 px-2 py-1 text-sm font-semibold bg-wash border border-line-strong hover:bg-logo-primary/10 rounded-md cursor-pointer hover:border-logo-primary"
        }
      >
        {capturing
          ? partial
            ? formatKeyCombination(partial, osType)
            : t("settings.advanced.voiceCommands.pressKeys")
          : command.keys
            ? formatKeyCombination(command.keys, osType)
            : t("settings.advanced.voiceCommands.setKeys")}
      </button>
      <label className="inline-flex items-center gap-1 text-sm cursor-pointer select-none">
        <input
          type="checkbox"
          className="accent-logo-primary"
          checked={endOnly}
          disabled={disabled}
          onChange={(e) => onChange({ ...command, end_only: e.target.checked })}
        />
        {t("settings.advanced.voiceCommands.endOnly")}
      </label>
      <Button
        onClick={onRemove}
        disabled={disabled}
        variant="secondary"
        size="sm"
        aria-label={t("settings.advanced.voiceCommands.remove", {
          phrase: command.phrase,
        })}
      >
        <svg
          className="w-3 h-3"
          fill="none"
          stroke="currentColor"
          viewBox="0 0 24 24"
        >
          <path
            strokeLinecap="round"
            strokeLinejoin="round"
            strokeWidth={2}
            d="M6 18L18 6M6 6l12 12"
          />
        </svg>
      </Button>
    </div>
  );
};

/**
 * Spoken phrases that press keys ("press enter" → Enter). Applied to the text
 * about to be pasted, so after post-processing.
 */
export const VoiceCommands: React.FC<VoiceCommandsProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const commands = getSetting("voice_commands") || [];
    const busy = isUpdating("voice_commands");

    const save = (next: VoiceCommand[]) =>
      updateSetting("voice_commands", next);

    const add = () =>
      save([
        ...commands,
        { id: newId(), phrase: "", keys: "", end_only: true },
      ]);

    return (
      <>
        <SettingContainer
          title={t("settings.advanced.voiceCommands.title")}
          description={t("settings.advanced.voiceCommands.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        >
          <Button
            onClick={add}
            disabled={busy}
            variant="primary"
            size="md"
            aria-label={t("settings.advanced.voiceCommands.add")}
            title={t("settings.advanced.voiceCommands.add")}
          >
            <svg
              className="w-4 h-4"
              fill="none"
              stroke="currentColor"
              viewBox="0 0 24 24"
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth={2}
                d="M12 5v14M5 12h14"
              />
            </svg>
          </Button>
        </SettingContainer>
        {commands.length > 0 && (
          <div
            className={`${grouped ? "" : "rounded-lg border border-line"} flex flex-col`}
          >
            {commands.map((command) => (
              <VoiceCommandRow
                key={command.id}
                command={command}
                disabled={busy}
                onChange={(next) =>
                  save(commands.map((c) => (c.id === next.id ? next : c)))
                }
                onRemove={() =>
                  save(commands.filter((c) => c.id !== command.id))
                }
              />
            ))}
          </div>
        )}
      </>
    );
  },
);
