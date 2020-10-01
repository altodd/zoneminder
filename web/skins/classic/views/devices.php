<?php
//
// ZoneMinder web devices file, $Date$, $Revision$
// Copyright (C) 2001-2008 Philip Coombes
//
// This program is free software; you can redistribute it and/or
// modify it under the terms of the GNU General Public License
// as published by the Free Software Foundation; either version 2
// of the License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program; if not, write to the Free Software
// Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA.
//

if ( !canView( 'Devices' ) )
{
    $view = "error";
     return;
}

$sql = "SELECT * FROM Devices WHERE Type = 'X10' ORDER BY Name";
$devices = array();
foreach( dbFetchAll( $sql ) as $row )
{
    $row['Status'] = getDeviceStatusX10( $row['KeyString'] );
    $devices[] = $row;
}

xhtmlHeaders(__FILE__, translate('Devices') );
?>
<body>
  <?php echo getNavBarHTML(); ?>
  <div id="page">
    <div class="w-100 py-1">
      <div class="float-left pl-3">
        <button type="button" id="backBtn" class="btn btn-normal" data-toggle="tooltip" data-placement="top" title="<?php echo translate('Back') ?>" disabled><i class="fa fa-arrow-left"></i></button>
        <button type="button" id="refreshBtn" class="btn btn-normal" data-toggle="tooltip" data-placement="top" title="<?php echo translate('Refresh') ?>" ><i class="fa fa-refresh"></i></button>
      </div>
      <div class="w-100 pt-2">
        <h2><?php echo translate('Devices') ?></h2>
      </div>
    </div>
    <div id="content">
      <form name="contentForm" method="get" action="?">
        <input type="hidden" name="view" value="none"/>
        <input type="hidden" name="action" value="device"/>
        <input type="hidden" name="key" value=""/>
        <input type="hidden" name="command" value=""/>
        <table id="contentTable" class="major" cellspacing="0">
          <tbody>
<?php
foreach( $devices as $device )
{
    if ( $device['Status'] == 'ON' )
    {
        $fclass = "deviceCol infoText";
    }
    elseif ( $device['Status'] == 'OFF' )
    {
        $fclass = "deviceCol warnText";
    }
    else
    {
        $fclass = "deviceCol errorText";
    }
?>
            <tr>
              <td><?php echo makeLink( '?view=device&amp;did='.$device['Id'], '<span class="'.$fclass.'">'.validHtmlStr($device['Name']).' ('.validHtmlStr($device['KeyString']).')</span>', canEdit( 'Devices' ) ) ?></td>
              <td><input type="button" value="<?php echo translate('On') ?>"<?php echo ($device['Status'] != 'ON')?' class="set"':'' ?> onclick="switchDeviceOn( this, '<?php echo validHtmlStr($device['KeyString']) ?>' )"<?php echo canEdit( 'Devices' )?"":' disabled="disabled"' ?>/></td>
              <td><input type="button" value="<?php echo translate('Off') ?>"<?php echo ($device['Status'] != 'OFF')?' class="set"':'' ?> onclick="switchDeviceOff( this, '<?php echo validHtmlStr($device['KeyString']) ?>' )"<?php echo canEdit( 'Devices' )?"":' disabled="disabled"' ?>/></td>
              <td><input type="checkbox" name="markDids[]" value="<?php echo $device['Id'] ?>" onclick="configureButtons( this, 'markDids' );"<?php if ( !canEdit( 'Devices' ) ) {?> disabled="disabled"<?php } ?>/></td>
            </tr>
<?php
}
?>
          </tbody>
        </table>
        <div id="contentButtons">
          <button type="button" id="newDeviceBtn" value="<?php echo translate('New') ?>" disabled="disabled"><?php echo translate('New') ?></button>
          <input type="button" class="btn-danger" name="deleteBtn" value="<?php echo translate('Delete') ?>" data-on-click-this="deleteDevice" disabled="disabled"/>
        </div>
      </form>
    </div>
  </div>
<?php xhtmlFooter() ?>
