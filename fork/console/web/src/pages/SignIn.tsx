import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import Container from "@cloudscape-design/components/container";
import ExpandableSection from "@cloudscape-design/components/expandable-section";
import Form from "@cloudscape-design/components/form";
import FormField from "@cloudscape-design/components/form-field";
import Header from "@cloudscape-design/components/header";
import Input from "@cloudscape-design/components/input";
import Select from "@cloudscape-design/components/select";
import SpaceBetween from "@cloudscape-design/components/space-between";
import Textarea from "@cloudscape-design/components/textarea";
import { useState } from "react";
import { apiContext } from "../api";
import { callerIdentity } from "../catalog";
import { ErrorAlert } from "../ErrorAlert";
import { DEFAULT_REGION, parseCredentialsFile, REGIONS, type Session } from "../session";

/** Asks for the emulator's keys and checks them with STS before keeping them. */
export function SignIn({ onSignIn }: { onSignIn: (session: Session, account: string | undefined) => void }) {
  const [accessKeyId, setAccessKeyId] = useState("");
  const [secretAccessKey, setSecretAccessKey] = useState("");
  const [region, setRegion] = useState(DEFAULT_REGION);
  const [pasted, setPasted] = useState("");
  const [error, setError] = useState<unknown>();
  const [checking, setChecking] = useState(false);

  const submit = async () => {
    setError(undefined);
    let keys = { accessKeyId: accessKeyId.trim(), secretAccessKey: secretAccessKey.trim() };
    try {
      if (pasted.trim() !== "") {
        keys = parseCredentialsFile(pasted);
      }
      if (keys.accessKeyId === "" || keys.secretAccessKey === "") {
        throw new Error("Enter an access key ID and a secret access key, or paste the credentials file.");
      }
      setChecking(true);
      const session = { ...keys, region };
      const identity = await callerIdentity(apiContext(session));
      onSignIn(session, identity.Account);
    } catch (caught) {
      setError(caught);
    } finally {
      setChecking(false);
    }
  };

  const port = globalThis.location.port || "9297";
  return (
    <Box padding={{ vertical: "xxxl", horizontal: "l" }}>
      <div style={{ maxWidth: 560, margin: "0 auto" }}>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
        >
          <Form
            header={<Header variant="h1" description="A read-only view of what this emulator holds.">AWS Emulator console</Header>}
            actions={
              <Button variant="primary" formAction="submit" loading={checking}>
                Sign in
              </Button>
            }
          >
            <Container header={<Header variant="h2">Emulator credentials</Header>}>
              <SpaceBetween size="l">
                <Box variant="p">
                  The emulator writes its keys to <code>~/.aws/emulator/{port}/emulator_credentials</code>. With{" "}
                  <code>--auth off</code>, any key pair works.
                </Box>
                {error !== undefined && <ErrorAlert error={error} action="Sign in failed" />}
                <FormField label="Access key ID">
                  <Input value={accessKeyId} onChange={(event) => setAccessKeyId(event.detail.value)} autoComplete={false} />
                </FormField>
                <FormField label="Secret access key">
                  <Input
                    type="password"
                    value={secretAccessKey}
                    onChange={(event) => setSecretAccessKey(event.detail.value)}
                    autoComplete={false}
                  />
                </FormField>
                <ExpandableSection headerText="Paste the credentials file instead">
                  <FormField description="The whole file; the [default] profile is used.">
                    <Textarea value={pasted} onChange={(event) => setPasted(event.detail.value)} rows={4} ariaLabel="Credentials file" />
                  </FormField>
                </ExpandableSection>
                <FormField label="Region">
                  <Select
                    selectedOption={{ value: region, label: region }}
                    options={REGIONS.map((value) => ({ value, label: value }))}
                    onChange={(event) => setRegion(event.detail.selectedOption.value ?? DEFAULT_REGION)}
                  />
                </FormField>
                <Box variant="small">The keys are kept for this browser tab only.</Box>
              </SpaceBetween>
            </Container>
          </Form>
        </form>
      </div>
    </Box>
  );
}
