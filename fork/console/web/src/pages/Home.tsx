import ColumnLayout from "@cloudscape-design/components/column-layout";
import Container from "@cloudscape-design/components/container";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Header from "@cloudscape-design/components/header";
import SpaceBetween from "@cloudscape-design/components/space-between";
import type { ApiContext } from "../api";
import { servicesByCategory } from "../catalog";
import { RouterLink } from "../RouterLink";
import { ResourceCount } from "./ResourceCount";

/** Every service, grouped by category, with the count of each resource type. */
export function Home({ context }: { context: ApiContext }) {
  return (
    <ContentLayout
      header={
        <Header variant="h1" description={`What the emulator at ${context.endpoint} holds in ${context.region}. Read only.`}>
          Console home
        </Header>
      }
    >
      <SpaceBetween size="xl">
        {[...servicesByCategory()].map(([category, services]) => (
          <SpaceBetween size="m" key={category}>
            <Header variant="h2">{category}</Header>
            {services.map((service) => (
              <Container key={service.id} header={<Header variant="h3"><RouterLink href={`/services/${service.id}`}>{service.name}</RouterLink></Header>}>
                <ColumnLayout columns={4} variant="text-grid">
                  {service.resources.map((resource) => (
                    <ResourceCount key={resource.id} context={context} service={service} resource={resource} />
                  ))}
                </ColumnLayout>
              </Container>
            ))}
          </SpaceBetween>
        ))}
      </SpaceBetween>
    </ContentLayout>
  );
}
