import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { useState } from 'react';
import GridDisclosure from './GridDisclosure';

function StatefulChild() {
  const [count, setCount] = useState(0);
  return <button onClick={() => setCount(count + 1)}>Count {count}</button>;
}

describe('GridDisclosure', () => {
  it('lazily mounts children and preserves their state across collapse', () => {
    const view = render(<GridDisclosure open={false}><StatefulChild /></GridDisclosure>);
    expect(screen.queryByText('Count 0')).toBeNull();
    view.rerender(<GridDisclosure open><StatefulChild /></GridDisclosure>);
    fireEvent.click(screen.getByRole('button'));
    view.rerender(<GridDisclosure open={false}><StatefulChild /></GridDisclosure>);
    expect(view.container.firstChild).toHaveAttribute('inert');
    expect(view.container.firstChild).toHaveAttribute('aria-hidden', 'true');
    expect(screen.queryByRole('button')).toBeNull();
    view.rerender(<GridDisclosure open><StatefulChild /></GridDisclosure>);
    expect(screen.getByRole('button')).toHaveTextContent('Count 1');
    expect(view.container.firstChild).not.toHaveAttribute('inert');
  });

  it('returns focus to the associated trigger on external collapse', () => {
    const panel = (open: boolean) => <><button aria-controls="details">Toggle</button><GridDisclosure id="details" open={open}><button>Inside</button></GridDisclosure></>;
    const view = render(panel(true));
    screen.getByRole('button', { name: 'Inside' }).focus();
    view.rerender(panel(false));
    expect(screen.getByRole('button', { name: 'Toggle' })).toHaveFocus();
    expect(view.container.querySelector('#details')).toHaveAttribute('data-open', 'false');
  });
});
