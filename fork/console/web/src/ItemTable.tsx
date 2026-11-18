import { useCollection } from "@cloudscape-design/collection-hooks";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import Header from "@cloudscape-design/components/header";
import Pagination from "@cloudscape-design/components/pagination";
import Table from "@cloudscape-design/components/table";
import TextFilter from "@cloudscape-design/components/text-filter";
import type { ReactNode } from "react";
import type { Column } from "./catalog";
import { ErrorAlert } from "./ErrorAlert";
import type { Item } from "./format";
import { RouterLink } from "./RouterLink";
import type { AsyncState } from "./useAsync";

interface Row {
  key: string;
  cells: string[];
}

export interface ItemTableProps {
  title: string;
  state: AsyncState<Item[]>;
  columns: Column[];
  itemKey: (item: Item) => string;
  /** Where a row's first cell links to; rows do not link when absent. */
  detailHref?: (key: string) => string;
  onRefresh: () => void;
  variant?: "full-page" | "container";
  description?: ReactNode;
}

/** A filterable, sortable, paged table of the items one list call returned. */
export function ItemTable({ title, state, columns, itemKey, detailHref, onRefresh, variant = "container", description }: ItemTableProps) {
  const rows: Row[] =
    state.status === "done" ? state.value.map((item) => ({ key: itemKey(item), cells: columns.map((column) => column.cell(item)) })) : [];
  const { items, filterProps, filteredItemsCount, paginationProps, collectionProps } = useCollection(rows, {
    filtering: {
      filteringFunction: (row, text) => row.cells.some((cell) => cell.toLowerCase().includes(text.toLowerCase())),
      empty: <Box textAlign="center" color="inherit">No {title.toLowerCase()} in this region.</Box>,
      noMatch: <Box textAlign="center" color="inherit">No {title.toLowerCase()} match the filter.</Box>,
    },
    pagination: { pageSize: 50 },
    sorting: {},
  });
  const count = state.status === "done" ? `(${state.value.length})` : undefined;
  return (
    <Table
      {...collectionProps}
      variant={variant}
      ariaLabels={{ tableLabel: title }}
      loading={state.status === "loading"}
      loadingText={`Loading ${title.toLowerCase()}`}
      items={items}
      trackBy="key"
      header={
        <Header
          variant={variant === "full-page" ? "awsui-h1-sticky" : "h2"}
          counter={count}
          description={description}
          actions={<Button iconName="refresh" ariaLabel={`Refresh ${title}`} onClick={onRefresh} />}
        >
          {title}
        </Header>
      }
      filter={
        <TextFilter
          {...filterProps}
          filteringPlaceholder={`Find ${title.toLowerCase()}`}
          countText={filterProps.filteringText ? `${filteredItemsCount ?? 0} matches` : undefined}
        />
      }
      pagination={<Pagination {...paginationProps} />}
      empty={state.status === "failed" ? <ErrorAlert error={state.error} action={`Listing ${title.toLowerCase()} failed`} /> : undefined}
      columnDefinitions={columns.map((column, index) => ({
        id: String(index),
        header: column.header,
        sortingComparator: (a: Row, b: Row) => (a.cells[index] ?? "").localeCompare(b.cells[index] ?? ""),
        cell: (row: Row) =>
          index === 0 && detailHref ? <RouterLink href={detailHref(row.key)}>{row.cells[0]}</RouterLink> : row.cells[index],
      }))}
    />
  );
}
