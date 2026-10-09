import { stripVTControlCharacters } from "node:util";
import { redactCommandOutput, redactSensitiveText } from "@ardurbot/logging";
export const safeText = (text: string) =>
  Array.from(stripVTControlCharacters(text))
    .filter((char) => {
      const code = char.codePointAt(0)!;
      return code === 9 || code === 10 || (code >= 32 && (code < 127 || code > 159));
    })
    .join("");

export const safeDiagnostic = (value: string) =>
  redactSensitiveText(redactCommandOutput(safeText(value)));
