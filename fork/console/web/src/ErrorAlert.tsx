import Alert from "@cloudscape-design/components/alert";
import { describeError } from "./api";

/** An API call that failed, with the error name and message the emulator returned. */
export function ErrorAlert({ error, action }: { error: unknown; action?: string }) {
  const { title, message } = describeError(error);
  return (
    <Alert type="error" header={action ? `${action}: ${title}` : title}>
      {message}
    </Alert>
  );
}
