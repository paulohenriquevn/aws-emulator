import Box from "@cloudscape-design/components/box";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Header from "@cloudscape-design/components/header";
import { RouterLink } from "../RouterLink";

export function NotFound() {
  return (
    <ContentLayout header={<Header variant="h1">Page not found</Header>}>
      <Box variant="p">
        The console has no page at this address. <RouterLink href="/">Go to the console home</RouterLink>.
      </Box>
    </ContentLayout>
  );
}
